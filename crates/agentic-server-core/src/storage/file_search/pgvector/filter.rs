//! Typed attribute predicates and bound PostgreSQL candidate parameters.
use crate::types::file_search::{
    AttributeValue, ComparisonOperator, CompoundOperator, FileSearchError, FilterValue, SearchFilter, invalid,
};

pub(super) fn push_filter(sql: &mut CandidateSql, filter: &SearchFilter) -> Result<(), FileSearchError> {
    sql.push("(");
    match filter {
        SearchFilter::Compound(compound) => {
            for (index, child) in compound.filters.iter().enumerate() {
                if index > 0 {
                    sql.push(match compound.operator {
                        CompoundOperator::And => " AND ",
                        CompoundOperator::Or => " OR ",
                    });
                }
                push_filter(sql, child)?;
            }
        }
        SearchFilter::Comparison(comparison) => {
            let attr = |sql: &mut CandidateSql| {
                sql.push("(data::jsonb -> 'attributes' -> ")
                    .push_bind(comparison.key.clone())
                    .push(")");
            };
            attr(sql);
            sql.push(" IS NOT NULL AND ");
            match &comparison.value {
                FilterValue::List(values) => {
                    attr(sql);
                    sql.push(if comparison.operator == ComparisonOperator::Nin {
                        " NOT IN ("
                    } else {
                        " IN ("
                    });
                    for (i, value) in values.iter().enumerate() {
                        if i > 0 {
                            sql.push(", ");
                        }
                        sql.push_bind(serde_json::to_string(value)?).push("::jsonb");
                    }
                    sql.push(")");
                }
                FilterValue::Scalar(value) => {
                    sql.push("jsonb_typeof(");
                    attr(sql);
                    sql.push(") = ").push_bind(match value {
                        AttributeValue::String(_) => "string",
                        AttributeValue::Number(_) => "number",
                        AttributeValue::Boolean(_) => "boolean",
                    });
                    sql.push(" AND ");
                    if matches!(value, AttributeValue::String(_)) {
                        sql.push("(data::jsonb -> 'attributes' ->> ")
                            .push_bind(comparison.key.clone())
                            .push(") COLLATE \"C\"");
                    } else {
                        attr(sql);
                    }
                    sql.push(match comparison.operator {
                        ComparisonOperator::Eq => " = ",
                        ComparisonOperator::Ne => " <> ",
                        ComparisonOperator::Gt => " > ",
                        ComparisonOperator::Gte => " >= ",
                        ComparisonOperator::Lt => " < ",
                        ComparisonOperator::Lte => " <= ",
                        ComparisonOperator::In | ComparisonOperator::Nin => return invalid("invalid scalar filter"),
                    });
                    if let AttributeValue::String(value) = value {
                        sql.push_bind(value.clone());
                    } else {
                        sql.push_bind(serde_json::to_string(value)?).push("::jsonb");
                    }
                }
            }
        }
    }
    sql.push(")");
    Ok(())
}

// sqlx Any uses question-mark QueryBuilder placeholders, whereas PostgreSQL
// requires numbered parameters. Only typed values enter the bound parameter list.
pub(super) struct CandidateSql {
    pub(super) text: String,
    pub(super) parameters: Vec<String>,
}
impl CandidateSql {
    pub(super) fn new(text: &str) -> Self {
        Self {
            text: text.into(),
            parameters: Vec::new(),
        }
    }
    pub(super) fn push(&mut self, text: impl AsRef<str>) -> &mut Self {
        self.text.push_str(text.as_ref());
        self
    }
    pub(super) fn push_bind(&mut self, value: impl Into<String>) -> &mut Self {
        self.parameters.push(value.into());
        self.text.push('$');
        self.text.push_str(&self.parameters.len().to_string());
        self
    }
}
