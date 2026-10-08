//! Pure SQL-to-SQL compilation; no catalogs, database connections, or global state.

use sqlparser::ast::{
    self, CastKind, DataType, Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments,
    Ident, Query, Select, Statement, TableFactor, Visit, Visitor,
};
use sqlparser::dialect::GenericDialect;
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

/// Command. Consume one declaration from the caller's parser.
/// Args: parser positioned after FUNCTION. Returns: name and parsed definition.
/// Example: twice(x) AS (x*2) -> parameter x and multiplication body.
fn declaration(parser: &mut Parser<'_>) -> Result<(String, Definition)> {
    let name = key(&parser.parse_identifier()?);
    parser.expect_token(&Token::LParen)?;
    let mut parameters = Vec::new();
    let mut seen = BTreeSet::new();
    if !parser.consume_token(&Token::RParen) {
        loop {
            let parameter = key(&parser.parse_identifier()?);
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
        match table {
            TableFactor::Table { name, alias, args, .. } if args.is_none() => {
                if let Some(alias) = alias { self.names.insert(key(&alias.name)); }
                else if let Some(identifier) = name.0.last().and_then(|part| part.as_ident()) {
                    self.names.insert(key(identifier));
                }
            }
            TableFactor::Derived { alias: Some(alias), .. } => { self.names.insert(key(&alias.name)); }
            _ => return ControlFlow::Break(Error::Invalid(
                "function subqueries support named tables and aliased derived tables; unsupported relation form".into())),
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
    let local = relations(&definition.body)?;
    let parameters: BTreeSet<_> = definition
        .parameters
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    finish(ast::visit_expressions(&definition.body, |expression| {
        let invalid = match expression {
            Expr::Lambda(_)
            | Expr::BinaryOp {
                op: ast::BinaryOperator::Arrow,
                ..
            } => Some(
                "lambda/JSON arrow syntax is not supported inside local function definitions"
                    .into(),
            ),
            Expr::Function(call) if matches!(call.over, Some(ast::WindowType::NamedWindow(_))) => {
                Some(
                    "named window references are not supported inside local function definitions"
                        .into(),
                )
            }
            Expr::Identifier(id) if !parameters.contains(key(id).as_str()) => Some(format!(
                "unbound identifier {id}; pass it as a parameter or qualify a subquery column"
            )),
            Expr::CompoundIdentifier(ids)
                if ids.len() > 1 && !local.names.contains(&key(&ids[0])) =>
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
    }))
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

struct Expander<'a> {
    definitions: &'a BTreeMap<String, Definition>,
    active: Vec<String>,
    calls: usize,
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
        validate_arguments(&values, &relations(&body)?)?;
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
        let flow = ast::visit_expressions_mut(&mut body, |item| {
            if let Expr::Identifier(id) = item
                && let Some(value) = replacements.get(&key(id))
            {
                *item = value.clone();
            }
            ControlFlow::<Error>::Continue(())
        });
        finish(flow)?;
        self.active.push(name);
        self.visit(&mut body)?;
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
/// Example: WITH FUNCTION f(x) AS(x+1) SELECT f(2) -> SELECT ((2) + 1).
pub fn expand(sql: &str) -> Result<String> {
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
        return Ok(sql.to_owned());
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
    let mut query = parser.parse_query()?;
    let _ = parser.consume_token(&Token::SemiColon);
    parser.expect_token(&Token::EOF)?;
    finish(query.visit(&mut QueryOnly))?;
    let mut expander = Expander {
        definitions: &definitions,
        active: Vec::new(),
        calls: 0,
    };
    expander.visit(&mut query)?;
    finish(query.visit(&mut QueryOnly))?;
    let output = query.to_string();
    if output.len() > MAX_OUTPUT_BYTES {
        return Err(Error::Invalid("expanded SQL exceeds 8 MiB".into()));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::expand;

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
}
