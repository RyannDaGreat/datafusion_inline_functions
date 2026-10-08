//! Pure SQL-to-SQL compilation; no catalogs, database connections, or global state.

use sqlparser::ast::{
    self, CastKind, DataType, Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments,
    Ident, Query, Select, SelectItem, SetExpr, Statement, TableFactor, Visit, Visitor, VisitorMut,
};
use sqlparser::dialect::{Dialect, GenericDialect};
use sqlparser::keywords::Keyword;
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::{Token, Tokenizer, TokenizerError};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;

// These caps bound recursive expansion and cloned AST growth, not database work.
const MAX_FUNCTION_DEPTH: usize = 32;
const MAX_EXPANSIONS: usize = 1024;
const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Parse(#[from] ParserError),
    #[error(transparent)]
    Tokenize(#[from] TokenizerError),
}

type Result<T> = std::result::Result<T, Error>;

#[derive(Clone)]
struct Parameter {
    name: String,
    data_type: Option<DataType>,
}

#[derive(Clone)]
struct Definition {
    parameters: Vec<Parameter>,
    return_type: Option<DataType>,
    body: Expr,
}

/// Pure function. Resolve SQL identifier case without changing quoted names.
/// Args: identifier. Returns: its comparison key. Example: unquoted Foo -> "foo".
fn key(identifier: &Ident) -> String {
    if identifier.quote_style.is_some() {
        identifier.value.clone()
    } else {
        identifier.value.to_lowercase()
    }
}

/// Pure function. Convert visitor termination into an ordinary error result.
/// Args: completed traversal. Returns: Ok(()) on Continue, otherwise its error.
/// Example: finish(ControlFlow::Continue(())) -> Ok(()).
fn finish(flow: ControlFlow<Error>) -> Result<()> {
    match flow {
        ControlFlow::Continue(()) => Ok(()),
        ControlFlow::Break(error) => Err(error),
    }
}

/// Pure function. Parenthesize an argument and enforce its optional SQL type.
/// Args: expression and optional type. Returns: grouped expression.
/// Example: expression 1+2, no type -> (1 + 2).
fn bound(expression: Expr, data_type: &Option<DataType>) -> Expr {
    if data_type.is_none() && numeric_constant(&expression) {
        // Keep one expression shape everywhere: GROUPING/ROLLUP match keys against projections.
        return Expr::Case {
            case_token: ast::helpers::attached_token::AttachedToken::empty(),
            end_token: ast::helpers::attached_token::AttachedToken::empty(),
            operand: None,
            conditions: vec![ast::CaseWhen {
                condition: Expr::Value(ast::Value::Boolean(true).into()),
                result: expression.clone(),
            }],
            else_result: Some(Box::new(expression)),
        };
    }
    if data_type.is_none() && matches!(expression, Expr::Value(_)) {
        return expression;
    }
    let expression = match data_type {
        Some(data_type) => Expr::Cast {
            kind: CastKind::Cast,
            expr: Box::new(expression),
            data_type: data_type.clone(),
            array: false,
            format: None,
        },
        None => expression,
    };
    Expr::Nested(Box::new(expression))
}

/// Pure function. Reject names that become SQL syntax instead of ordinary calls.
/// Args: declared identifier. Returns: Ok for callable syntax, otherwise a clear error.
/// Example: left -> Ok; all and any -> reserved-syntax errors.
fn validate_name(identifier: &Ident) -> Result<()> {
    let name = key(identifier);
    // These constructors can fall back to ordinary calls when their special syntax is absent.
    if matches!(
        name.as_str(),
        "cast"
            | "try_cast"
            | "safe_cast"
            | "convert"
            | "try_convert"
            | "extract"
            | "ceil"
            | "floor"
            | "position"
            | "substr"
            | "substring"
            | "overlay"
            | "trim"
            | "struct"
            | "array"
            | "exists"
            | "match"
            | "interval"
            | "case"
            | "not"
            | "true"
            | "false"
            | "null"
    ) {
        return Err(Error::Invalid(format!(
            "local function name {name:?} is reserved SQL syntax"
        )));
    }
    let dialect = GenericDialect {};
    let bare = identifier
        .value
        .chars()
        .enumerate()
        .all(|(index, character)| {
            if index == 0 {
                dialect.is_identifier_start(character)
            } else {
                dialect.is_identifier_part(character)
            }
        });
    let spelling = if identifier.quote_style.is_none()
        || (bare && identifier.value == identifier.value.to_lowercase())
    {
        name.clone()
    } else {
        identifier.to_string()
    };
    // Quantifiers and SELECT modifiers differ from calls only in certain contexts.
    for prefix in [
        "SELECT ",
        "SELECT 1 + ",
        "SELECT 1 = ",
        "SELECT 1 GROUP BY ",
    ] {
        let source = format!("{prefix}{spelling}(0)");
        let statements = Parser::parse_sql(&dialect, &source).map_err(|_| {
            Error::Invalid(format!(
                "local function name {name:?} is reserved SQL syntax"
            ))
        })?;
        let mut calls = 0;
        finish(ast::visit_expressions(&statements, |expression| {
            if let Expr::Function(call) = expression
                && call.name.0.len() == 1
                && call.name.0[0].as_ident().is_some_and(|id| key(id) == name)
            {
                calls += 1;
            }
            ControlFlow::<Error>::Continue(())
        }))?;
        if calls != 1 {
            return Err(Error::Invalid(format!(
                "local function name {name:?} is reserved SQL syntax"
            )));
        }
    }
    Ok(())
}

/// Command. Consume one declaration from the caller's parser.
/// Args: parser positioned after FUNCTION. Returns: name and parsed definition.
/// Example: twice(x) AS (x*2) -> parameter x and multiplication body.
fn declaration(parser: &mut Parser<'_>) -> Result<(String, Definition)> {
    let identifier = parser.parse_identifier()?;
    validate_name(&identifier)?;
    let name = key(&identifier);
    parser.expect_token(&Token::LParen)?;
    let mut parameters = Vec::new();
    let mut seen = BTreeSet::new();
    if !parser.consume_token(&Token::RParen) {
        loop {
            let identifier = parser.parse_identifier()?;
            let spelling = identifier.to_string();
            let probe = Parser::new(&GenericDialect {})
                .try_with_sql(&spelling)?
                .parse_expr();
            let contextual = Parser::new(&GenericDialect {})
                .try_with_sql(&format!("{spelling} + 1"))?
                .parse_expr();
            if !matches!(probe, Ok(Expr::Identifier(ref parsed)) if key(parsed) == key(&identifier))
                || !matches!(contextual, Ok(Expr::BinaryOp { ref left, .. })
                    if matches!(left.as_ref(), Expr::Identifier(parsed) if key(parsed) == key(&identifier)))
            {
                return Err(Error::Invalid(format!(
                    "parameter {spelling} is reserved SQL syntax; use a different or quoted name"
                )));
            }
            let parameter = key(&identifier);
            if !seen.insert(parameter.clone()) {
                return Err(Error::Invalid(format!(
                    "duplicate parameter {parameter:?} in {name}"
                )));
            }
            let data_type = if matches!(parser.peek_token().token, Token::Comma | Token::RParen) {
                None
            } else {
                Some(parser.parse_data_type()?)
            };
            parameters.push(Parameter {
                name: parameter,
                data_type,
            });
            if parser.consume_token(&Token::RParen) {
                break;
            }
            parser.expect_token(&Token::Comma)?;
        }
    }
    let (return_type, body) = if parser.parse_keyword(Keyword::RETURNS) {
        let data_type = parser.parse_data_type()?;
        parser.expect_keyword(Keyword::RETURN)?;
        (Some(data_type), parser.parse_expr()?)
    } else {
        parser.expect_keyword(Keyword::AS)?;
        parser.expect_token(&Token::LParen)?;
        let body = parser.parse_expr()?;
        parser.expect_token(&Token::RParen)?;
        (None, body)
    };
    Ok((
        name,
        Definition {
            parameters,
            return_type,
            body,
        },
    ))
}

#[derive(Default)]
struct Relations {
    has_query: bool,
    names: BTreeSet<String>,
    allow_other_forms: bool,
}

impl Visitor for Relations {
    type Break = Error;

    /// Command. Mark that the visited body contains a query; collect CTE names.
    /// Args: query. Returns: Continue; mutates this collector only.
    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<Error> {
        self.has_query = true;
        if let Some(with) = &query.with {
            self.names
                .extend(with.cte_tables.iter().map(|cte| key(&cte.alias.name)));
        }
        ControlFlow::Continue(())
    }

    /// Command. Collect body-local aliases, rejecting unsupported relation forms.
    /// Args: relation. Returns: Continue or explicit unsupported-binding error.
    fn pre_visit_table_factor(&mut self, table: &TableFactor) -> ControlFlow<Error> {
        if !self.allow_other_forms
            && !matches!(
                table,
                TableFactor::Table { args: None, .. } | TableFactor::Derived { alias: Some(_), .. }
            )
        {
            return ControlFlow::Break(Error::Invalid(
                "function subqueries support named tables and aliased derived tables; unsupported relation form".into()));
        }
        let alias = match table {
            TableFactor::Table { alias, .. }
            | TableFactor::Derived { alias, .. }
            | TableFactor::TableFunction { alias, .. }
            | TableFactor::Function { alias, .. }
            | TableFactor::UNNEST { alias, .. }
            | TableFactor::JsonTable { alias, .. }
            | TableFactor::OpenJsonTable { alias, .. }
            | TableFactor::NestedJoin { alias, .. }
            | TableFactor::Pivot { alias, .. }
            | TableFactor::Unpivot { alias, .. }
            | TableFactor::MatchRecognize { alias, .. }
            | TableFactor::XmlTable { alias, .. }
            | TableFactor::SemanticView { alias, .. } => alias,
        };
        if let Some(alias) = alias {
            self.names.insert(key(&alias.name));
        } else if let TableFactor::Table { name, .. }
        | TableFactor::Function { name, .. }
        | TableFactor::SemanticView { name, .. } = table
            && let Some(identifier) = name.0.last().and_then(|part| part.as_ident())
        {
            self.names.insert(key(identifier));
        }
        ControlFlow::Continue(())
    }
}

/// Pure function. Describe relation names introduced inside an expression body.
/// Args: expression. Returns: local relation names and whether it contains SELECT.
/// Example: (SELECT count(*) FROM windows w) -> has_query=true, names={w}.
fn relations(body: &Expr) -> Result<Relations> {
    let mut result = Relations::default();
    finish(body.visit(&mut result))?;
    Ok(result)
}

/// Pure function. Check that a definition binds its names independently of callers.
/// Args: declaration body and parameters. Returns: Ok or explicit free-name error.
/// Example: f(x) AS(x+1) is valid; f(x) AS(x+y) rejects free identifier y.
fn validate_definition(definition: &Definition) -> Result<()> {
    finish(definition.body.visit(&mut QueryOnly))?;
    relations(&definition.body)?;
    finish(
        definition.body.visit(&mut DefinitionScope {
            parameters: definition
                .parameters
                .iter()
                .map(|p| p.name.as_str())
                .collect(),
            scopes: Vec::new(),
        }),
    )
}

struct DefinitionScope<'a> {
    parameters: BTreeSet<&'a str>,
    scopes: Vec<BTreeSet<String>>,
}

/// Pure function. Recognize both SQL spellings of a named window reference.
/// Args: function call. Returns: whether OVER names another window.
/// Example: sum(x) OVER (w ORDER BY x) -> true; sum(x) OVER () -> false.
fn has_named_window(call: &Function) -> bool {
    matches!(&call.over, Some(ast::WindowType::NamedWindow(_)))
        || matches!(&call.over, Some(ast::WindowType::WindowSpec(spec)) if spec.window_name.is_some())
}

impl Visitor for DefinitionScope<'_> {
    type Break = Error;

    /// Command. Enter an empty query scope before its CTEs are visited.
    /// Args: query. Returns: Continue or an unsupported-scope error; mutates scope stack.
    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<Error> {
        if !matches!(query.body.as_ref(), SetExpr::Select(_) | SetExpr::Values(_)) {
            return ControlFlow::Break(Error::Invalid(
                "function subqueries support SELECT or VALUES bodies, not set operations or parenthesized query bodies".into()));
        }
        self.scopes.push(BTreeSet::new());
        ControlFlow::Continue(())
    }

    /// Command. Leave the current query without exposing its names to siblings.
    /// Args: completed query. Returns: Continue; pops the private scope stack.
    fn post_visit_query(&mut self, _query: &Query) -> ControlFlow<Error> {
        self.scopes.pop();
        ControlFlow::Continue(())
    }

    /// Command. Bind this SELECT's immediate relations, excluding nested queries.
    /// Args: select. Returns: Continue or an unsupported wildcard/relation error.
    fn pre_visit_select(&mut self, select: &Select) -> ControlFlow<Error> {
        if select
            .projection
            .iter()
            .any(|item| matches!(item, SelectItem::QualifiedWildcard(..)))
        {
            return ControlFlow::Break(Error::Invalid(
                "qualified wildcards are not supported in function bodies".into(),
            ));
        }
        let mut local = Relations::default();
        for from in &select.from {
            local.pre_visit_table_factor(&from.relation)?;
            for join in &from.joins {
                local.pre_visit_table_factor(&join.relation)?;
            }
        }
        if let Some(scope) = self.scopes.last_mut() {
            *scope = local.names;
        }
        ControlFlow::Continue(())
    }

    /// Query. Check names against parameters and the active lexical query scopes.
    /// Args: expression. Returns: Continue or an explicit binding error; reads scope stack.
    fn pre_visit_expr(&mut self, expression: &Expr) -> ControlFlow<Error> {
        let invalid = match expression {
            Expr::QualifiedWildcard(..) => Some("qualified wildcards are not supported in function bodies".into()),
            Expr::Function(call) if [&call.args, &call.parameters].iter().any(|args| {
                matches!(args, FunctionArguments::List(list) if list.args.iter().any(|arg| matches!(arg,
                    FunctionArg::Unnamed(FunctionArgExpr::QualifiedWildcard(_))
                    | FunctionArg::Named { arg: FunctionArgExpr::QualifiedWildcard(_), .. }
                    | FunctionArg::ExprNamed { arg: FunctionArgExpr::QualifiedWildcard(_), .. })))
            }) => Some("qualified wildcards are not supported in function bodies".into()),
            Expr::Lambda(_)
            | Expr::BinaryOp {
                op: ast::BinaryOperator::Arrow,
                ..
            } => Some(
                "lambda/JSON arrow syntax is not supported inside local function definitions"
                    .into(),
            ),
            Expr::Function(call) if has_named_window(call) => {
                Some(
                    "named window references are not supported inside local function definitions"
                        .into(),
                )
            }
            Expr::Identifier(id) if !self.parameters.contains(key(id).as_str()) => Some(format!(
                "unbound identifier {id}; pass it as a parameter or qualify a subquery column"
            )),
            Expr::CompoundIdentifier(ids)
                if ids.len() > 1 && !self.scopes.iter().rev().any(|scope| scope.contains(&key(&ids[0]))) =>
            {
                Some(format!(
                    "unbound qualifier {}; function bodies may reference only their own relations",
                    ids[0]
                ))
            }
            _ => None,
        };
        match invalid {
            Some(message) => ControlFlow::Break(Error::Invalid(message)),
            None => ControlFlow::Continue(()),
        }
    }
}

/// Pure function. Ensure inserting arguments cannot capture callee table names.
/// Args: arguments and body-local relations. Returns: Ok or a binding error.
/// Example: outer.id into a body declaring outer is rejected; c.id into w is safe.
fn validate_arguments(arguments: &[Expr], local: &Relations) -> Result<()> {
    if !local.has_query {
        return Ok(());
    }
    finish(ast::visit_expressions(&arguments.to_vec(), |expression| {
        let invalid = match expression {
            Expr::Function(call) if call.over.is_some() => Some(
                "window expressions cannot be inserted into a function subquery; compute the window result in a caller CTE first".into()),
            Expr::Function(_) => Some(
                "function-call arguments cannot be inserted into a function subquery; compute the result in a caller CTE first".into()),
            Expr::Identifier(id) => Some(format!(
                "argument column {id} must be qualified when the function contains a subquery")),
            Expr::CompoundIdentifier(ids) if ids.iter().take(ids.len().saturating_sub(1)).any(|id| local.names.contains(&key(id))) => Some(
                "argument qualifier collides with a function-local relation; rename the caller or callee alias".into()),
            _ => None,
        };
        match invalid {
            Some(message) => ControlFlow::Break(Error::Invalid(message)),
            None => ControlFlow::Continue(()),
        }
    }))
}

/// Pure function. Extract positional scalar arguments and reject ignored modifiers.
/// Args: call. Returns: argument expressions or an unsupported-call error.
/// Example: f(1, x+2) -> two expressions; f(DISTINCT x) is rejected.
fn arguments(call: &Function) -> Result<Vec<Expr>> {
    if call.filter.is_some()
        || call.over.is_some()
        || call.null_treatment.is_some()
        || !call.within_group.is_empty()
        || !matches!(call.parameters, FunctionArguments::None)
        || call.uses_odbc_syntax
    {
        return Err(Error::Invalid(
            "local function calls do not support FILTER, OVER, or other call modifiers".into(),
        ));
    }
    let FunctionArguments::List(list) = &call.args else {
        return Err(Error::Invalid(
            "local functions require parentheses and positional expression arguments".into(),
        ));
    };
    if list.duplicate_treatment.is_some() || !list.clauses.is_empty() {
        return Err(Error::Invalid(
            "local function arguments do not support DISTINCT or argument clauses".into(),
        ));
    }
    list.args.iter().map(|arg| match arg {
        FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => Ok(expr.clone()),
        _ => Err(Error::Invalid("local functions accept only positional scalar expressions, not named arguments or wildcards".into())),
    }).collect()
}

/// Pure function. Identify numeric constants that SQL positional clauses can reinterpret.
/// Args: expression. Returns: true for numeric literals wrapped in parentheses or signs.
/// Example: (+(1)) -> true; f(1), a column, and CAST(1 AS INT) -> false.
fn numeric_constant(expression: &Expr) -> bool {
    match expression {
        Expr::Value(value) => matches!(value.value, ast::Value::Number(..)),
        Expr::Nested(inner)
        | Expr::UnaryOp {
            op: ast::UnaryOperator::Plus | ast::UnaryOperator::Minus,
            expr: inner,
        } => numeric_constant(inner),
        _ => false,
    }
}

/// Command. Restore compiler-generated numeric constants where DataFusion requires frame literals.
/// Args: mutable window specification. Returns: nothing; mutates its numeric frame bounds only.
/// Example: generated CASE(true, 1, 1) PRECEDING -> 1 PRECEDING; a user-written CASE stays unchanged.
fn restore_frame_literals(window: &mut ast::WindowSpec) {
    if let Some(frame) = &mut window.window_frame {
        for bound in std::iter::once(&mut frame.start_bound).chain(frame.end_bound.iter_mut()) {
            let (ast::WindowFrameBound::Preceding(Some(expression))
            | ast::WindowFrameBound::Following(Some(expression))) = bound
            else {
                continue;
            };
            let mut inner = expression.as_ref();
            while let Expr::Nested(nested) = inner {
                inner = nested;
            }
            if let Expr::Case {
                case_token,
                operand: None,
                conditions,
                else_result: Some(other),
                ..
            } = inner
                && case_token.0.span == sqlparser::tokenizer::Span::empty()
                && conditions.len() == 1
                && matches!(&conditions[0].condition, Expr::Value(value) if value.value == ast::Value::Boolean(true))
                && conditions[0].result == **other
                && numeric_constant(other)
            {
                **expression = conditions[0].result.clone();
            }
        }
    }
}

struct FrameLiterals;

impl VisitorMut for FrameLiterals {
    type Break = Error;

    /// Command. Restore numeric literal offsets in inline window specifications.
    /// Args: mutable expression. Returns: Continue; only compiler-generated frame wrappers change.
    fn post_visit_expr(&mut self, expression: &mut Expr) -> ControlFlow<Error> {
        if let Expr::Function(call) = expression
            && let Some(ast::WindowType::WindowSpec(window)) = &mut call.over
        {
            restore_frame_literals(window);
        }
        ControlFlow::Continue(())
    }

    /// Command. Restore numeric literal offsets in the caller's named windows.
    /// Args: mutable SELECT. Returns: Continue; only compiler-generated frame wrappers change.
    fn post_visit_select(&mut self, select: &mut Select) -> ControlFlow<Error> {
        for definition in &mut select.named_window {
            if let ast::NamedWindowExpr::WindowSpec(window) = &mut definition.1 {
                restore_frame_literals(window);
            }
        }
        ControlFlow::Continue(())
    }
}

struct Expander<'a> {
    definitions: &'a BTreeMap<String, Definition>,
    active: Vec<String>,
    calls: usize,
    relation_scopes: Vec<BTreeSet<String>>,
}

impl Expander<'_> {
    /// Command. Expand calls in the supplied AST and update this query's budget.
    /// Args: mutable AST. Returns: Ok after replacement, or explicit error.
    fn visit<T: ast::VisitMut>(&mut self, tree: &mut T) -> Result<()> {
        finish(ast::visit_expressions_mut(tree, |expression| {
            match self.replace(expression) {
                Ok(()) => ControlFlow::Continue(()),
                Err(error) => ControlFlow::Break(error),
            }
        }))
    }

    /// Command. Replace one local call; copied bodies and query-local budget mutate.
    /// Args: expression. Returns: Ok for a replaced call or untouched ordinary expression.
    fn replace(&mut self, expression: &mut Expr) -> Result<()> {
        let Expr::Function(call) = expression else {
            return Ok(());
        };
        if call.name.0.len() != 1 {
            return Ok(());
        }
        let Some(identifier) = call.name.0[0].as_ident() else {
            return Ok(());
        };
        let name = key(identifier);
        let Some(definition) = self.definitions.get(&name).cloned() else {
            return Ok(());
        };
        if self.active.contains(&name) {
            return Err(Error::Invalid(format!("recursive local function {name}")));
        }
        if self.active.len() >= MAX_FUNCTION_DEPTH || self.calls >= MAX_EXPANSIONS {
            return Err(Error::Invalid(
                "local function expansion limit exceeded".into(),
            ));
        }
        self.calls += 1;
        let values = arguments(call)?;
        if values.len() != definition.parameters.len() {
            return Err(Error::Invalid(format!(
                "{name} expects {} arguments, got {}",
                definition.parameters.len(),
                values.len()
            )));
        }
        let mut body = definition.body.clone();
        let local = relations(&body)?;
        if self
            .relation_scopes
            .iter()
            .any(|scope| !scope.is_disjoint(&local.names))
        {
            return Err(Error::Invalid("function-local relation collides with a caller relation; rename the caller or callee alias".into()));
        }
        validate_arguments(&values, &local)?;
        let replacements: BTreeMap<_, _> = definition
            .parameters
            .iter()
            .zip(values.iter())
            .map(|(parameter, value)| {
                (
                    parameter.name.clone(),
                    bound(value.clone(), &parameter.data_type),
                )
            })
            .collect();
        finish(ast::visit_expressions_mut(&mut body, |item| {
            if let Expr::Identifier(id) = item
                && let Some(value) = replacements.get(&key(id))
            {
                *item = value.clone();
            }
            ControlFlow::<Error>::Continue(())
        }))?;
        self.active.push(name);
        self.relation_scopes.push(local.names);
        self.visit(&mut body)?;
        self.relation_scopes.pop();
        self.active.pop();
        // Nested functions can introduce additional relation names after substitution.
        validate_arguments(&values, &relations(&body)?)?;
        *expression = bound(body, &definition.return_type);
        if expression.to_string().len() > MAX_OUTPUT_BYTES {
            return Err(Error::Invalid("expanded expression exceeds 8 MiB".into()));
        }
        Ok(())
    }
}

struct QueryOnly;

impl Visitor for QueryOnly {
    type Break = Error;

    /// Command. Validate visited query locks; the AST remains unchanged.
    /// Args: query. Returns: Continue for unlocked queries, otherwise an error.
    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<Error> {
        if query.locks.is_empty() {
            ControlFlow::Continue(())
        } else {
            ControlFlow::Break(Error::Invalid("locking queries are not supported".into()))
        }
    }

    /// Command. Reject SELECT INTO without modifying the visited select.
    /// Args: select. Returns: Continue for a read query, otherwise an error.
    fn pre_visit_select(&mut self, select: &Select) -> ControlFlow<Error> {
        if select.into.is_none() {
            ControlFlow::Continue(())
        } else {
            ControlFlow::Break(Error::Invalid("SELECT INTO is not supported".into()))
        }
    }

    /// Command. Reject statement-bearing mutation bodies inside a query AST.
    /// Args: statement. Returns: Continue only for query statements.
    fn pre_visit_statement(&mut self, statement: &Statement) -> ControlFlow<Error> {
        if matches!(statement, Statement::Query(_)) {
            ControlFlow::Continue(())
        } else {
            ControlFlow::Break(Error::Invalid(
                "function queries must not contain mutation statements".into(),
            ))
        }
    }
}

/// Pure function. Expand explicit top-level declarations; preserve other SQL exactly.
/// Args: SQL string. Returns: expanded query or original text, without executing it.
/// Example: WITH FUNCTION f(x) AS(x+1) SELECT f(2) -> SELECT (CASE WHEN true THEN 2 ELSE 2 END + 1).
pub fn expand(sql: &str) -> Result<String> {
    expand_with_offset(sql).map(|(expanded, _)| expanded)
}

/// Pure function. Expand declarations and locate the original main query in Unicode characters.
/// Args: SQL string. Returns: expanded SQL and zero-based character offset, or zero for pass-through.
/// Example: WITH FUNCTION f() AS('x') SELECT f() -> ("SELECT 'x'", 26).
pub fn expand_with_offset(sql: &str) -> Result<(String, usize)> {
    expand_with_metadata(sql).map(|(expanded, offset, _)| (expanded, offset))
}

/// Pure function. Expand SQL and locate the original query and its local calls.
/// Args: SQL text. Returns: expanded SQL, query offset, absolute Unicode call-name offsets.
/// Example: SELECT 1 -> ("SELECT 1", 0, []); only actual local calls contribute offsets.
pub fn expand_with_metadata(sql: &str) -> Result<(String, usize, Vec<usize>)> {
    let dialect = GenericDialect {};
    let mut tokens = Vec::new();
    let tokenized = Tokenizer::new(&dialect, sql).tokenize_with_location_into_buf(&mut tokens);
    let prefix: Vec<_> = tokens
        .iter()
        .filter(|t| !matches!(t.token, Token::Whitespace(_)))
        .take(2)
        .collect();
    let opted_in = prefix.len() == 2
        && matches!(&prefix[0].token, Token::Word(w) if w.keyword == Keyword::WITH)
        && matches!(&prefix[1].token, Token::Word(w) if w.keyword == Keyword::FUNCTION);
    // Unextended SQL belongs to the engine, including syntax this tokenizer cannot recognize.
    if !opted_in {
        return Ok((sql.to_owned(), 0, Vec::new()));
    }
    tokenized?;
    let mut parser = Parser::new(&dialect).with_tokens_with_locations(tokens);
    parser.expect_keywords(&[Keyword::WITH, Keyword::FUNCTION])?;
    let mut definitions = BTreeMap::new();
    loop {
        let (name, definition) = declaration(&mut parser)?;
        validate_definition(&definition)?;
        if definitions.insert(name.clone(), definition).is_some() {
            return Err(Error::Invalid(format!("duplicate local function {name}")));
        }
        if !parser.consume_token(&Token::Comma) {
            break;
        }
        parser.expect_keyword(Keyword::FUNCTION)?;
    }
    let query_start = parser.peek_token().span.start;
    let mut query = parser.parse_query()?;
    // sqlparser counts Unicode characters, resets only at LF, and counts CR/tab as one column.
    let mut line_offsets = vec![0];
    for line in sql.split_inclusive('\n') {
        line_offsets.push(line_offsets.last().unwrap() + line.chars().count());
    }
    let query_offset =
        line_offsets[query_start.line as usize - 1] + query_start.column as usize - 1;
    let mut call_offsets = Vec::new();
    finish(ast::visit_expressions(&query, |expression| {
        if let Expr::Function(call) = expression
            && call.name.0.len() == 1
            && let Some(identifier) = call.name.0[0].as_ident()
            && definitions.contains_key(&key(identifier))
        {
            let start = identifier.span.start;
            call_offsets.push(line_offsets[start.line as usize - 1] + start.column as usize - 1);
        }
        ControlFlow::<Error>::Continue(())
    }))?;
    let _ = parser.consume_token(&Token::SemiColon);
    parser.expect_token(&Token::EOF)?;
    finish(query.visit(&mut QueryOnly))?;
    let mut caller = Relations {
        allow_other_forms: true,
        ..Relations::default()
    };
    finish(query.visit(&mut caller))?;
    let mut expander = Expander {
        definitions: &definitions,
        active: Vec::new(),
        calls: 0,
        relation_scopes: vec![caller.names],
    };
    expander.visit(&mut query)?;
    finish(ast::VisitMut::visit(&mut query, &mut FrameLiterals))?;
    finish(query.visit(&mut QueryOnly))?;
    let output = query.to_string();
    if output.len() > MAX_OUTPUT_BYTES {
        return Err(Error::Invalid("expanded SQL exceeds 8 MiB".into()));
    }
    Ok((output, query_offset, call_offsets))
}

#[cfg(test)]
mod tests {
    use super::{expand, expand_with_offset};

    /// Command. Assert deterministic expansion, grouping, and byte-preserving pass-through.
    #[test]
    fn smoke() {
        assert_eq!(
            expand("-- comment\nSELECT 'weird; syntax' ;").unwrap(),
            "-- comment\nSELECT 'weird; syntax' ;"
        );
        assert_eq!(
            expand("WITH FUNCTION twice(x) AS (x*2) SELECT twice(3+1)").unwrap(),
            "SELECT ((3 + 1) * 2)"
        );
        assert!(expand("WITH FUNCTION f(x) AS (x+1) SELECT f(1,2)").is_err());
        assert!(expand("WITH FUNCTION f(x) AS (f(x)) SELECT f(1)").is_err());
    }

    /// Command. Verify source offsets count Unicode characters and all whitespace exactly.
    #[test]
    fn original_query_offsets() {
        for separator in [" ", "\n", "\r\n\t", "\r", "\t/* λ💡 */\n"] {
            let prefix = format!("-- λ💡\nWITH FUNCTION f(x) AS(x+1){separator}");
            let sql = format!("{prefix}SELECT f(2)");
            let (expanded, offset) = expand_with_offset(&sql).unwrap();
            assert_eq!(offset, prefix.chars().count());
            assert_eq!(expanded, "SELECT (CASE WHEN true THEN 2 ELSE 2 END + 1)");
            assert_eq!(sql.chars().skip(offset).collect::<String>(), "SELECT f(2)");
        }
        let prefix = "WITH FUNCTION f() AS('λ💡') /* note */ ";
        let query = "WITH q AS(SELECT 1) SELECT f() FROM q";
        assert_eq!(
            expand_with_offset(&format!("{prefix}{query}")).unwrap().1,
            prefix.chars().count()
        );
        assert_eq!(
            expand_with_offset("-- λ\r\nSELECT 1").unwrap(),
            ("-- λ\r\nSELECT 1".into(), 0)
        );
        assert!(expand_with_offset("WITH FUNCTION f() AS(1)").is_err());
    }
}
