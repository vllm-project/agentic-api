//! Attribute filter validation.

use super::{
    AttributeValue, ComparisonOperator, CompoundOperator, FileAttributes, FileSearchError, FilterValue, SearchFilter,
    invalid, validate_attribute_value,
};

impl SearchFilter {
    pub(super) fn validate_at(&self, depth: usize, nodes: &mut usize) -> Result<(), FileSearchError> {
        *nodes += 1;
        if depth > 8 || *nodes > 64 {
            return invalid("filters exceed the maximum depth of 8 or 64 total nodes");
        }
        match self {
            Self::Compound(filter) => {
                if filter.filters.is_empty() {
                    return invalid("compound filters must contain at least one filter");
                }
                for child in &filter.filters {
                    child.validate_at(depth + 1, nodes)?;
                }
            }
            Self::Comparison(filter) => {
                if filter.key.is_empty() || filter.key.len() > 64 {
                    return invalid("filter keys must contain 1 to 64 bytes");
                }
                match (&filter.operator, &filter.value) {
                    (ComparisonOperator::In | ComparisonOperator::Nin, FilterValue::List(values))
                        if !values.is_empty() && values.len() <= 64 =>
                    {
                        for value in values {
                            validate_attribute_value(value)?;
                        }
                    }
                    (ComparisonOperator::Eq | ComparisonOperator::Ne, FilterValue::Scalar(value)) => {
                        validate_attribute_value(value)?;
                    }
                    (
                        ComparisonOperator::Gt
                        | ComparisonOperator::Gte
                        | ComparisonOperator::Lt
                        | ComparisonOperator::Lte,
                        FilterValue::Scalar(value @ (AttributeValue::String(_) | AttributeValue::Number(_))),
                    ) => validate_attribute_value(value)?,
                    _ => return invalid("filter value does not match its comparison operator"),
                }
            }
        }
        Ok(())
    }

    pub(crate) fn matches(&self, attributes: &FileAttributes) -> bool {
        match self {
            Self::Compound(filter) => match filter.operator {
                CompoundOperator::And => filter.filters.iter().all(|filter| filter.matches(attributes)),
                CompoundOperator::Or => filter.filters.iter().any(|filter| filter.matches(attributes)),
            },
            Self::Comparison(filter) => {
                let Some(actual) = attributes.get(&filter.key) else {
                    return false;
                };
                match (&filter.operator, &filter.value) {
                    (ComparisonOperator::Eq, FilterValue::Scalar(value)) => actual == value,
                    (ComparisonOperator::Ne, FilterValue::Scalar(value)) => same_type(actual, value) && actual != value,
                    (ComparisonOperator::In, FilterValue::List(values)) => values.contains(actual),
                    (ComparisonOperator::Nin, FilterValue::List(values)) => !values.contains(actual),
                    (operator, FilterValue::Scalar(value)) => {
                        let ordering = match (actual, value) {
                            (AttributeValue::Number(left), AttributeValue::Number(right)) => left.partial_cmp(right),
                            (AttributeValue::String(left), AttributeValue::String(right)) => Some(left.cmp(right)),
                            _ => None,
                        };
                        ordering.is_some_and(|ordering| match operator {
                            ComparisonOperator::Gt => ordering.is_gt(),
                            ComparisonOperator::Gte => !ordering.is_lt(),
                            ComparisonOperator::Lt => ordering.is_lt(),
                            ComparisonOperator::Lte => !ordering.is_gt(),
                            _ => false,
                        })
                    }
                    _ => false,
                }
            }
        }
    }
}

fn same_type(left: &AttributeValue, right: &AttributeValue) -> bool {
    std::mem::discriminant(left) == std::mem::discriminant(right)
}
