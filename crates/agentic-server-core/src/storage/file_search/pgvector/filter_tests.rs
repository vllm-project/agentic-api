//! Differential coverage of PostgreSQL pushdown and the shared Rust matcher.

use super::*;
use crate::{
    storage::{create_pool, file_search::StoredChunk},
    types::file_search::{ComparisonFilter, CompoundFilter, FileAttributes, SearchQuery, SearchRequest},
};

fn comparison(key: &str, operator: ComparisonOperator, value: FilterValue) -> SearchFilter {
    SearchFilter::Comparison(ComparisonFilter {
        key: key.into(),
        operator,
        value,
    })
}

fn compound(operator: CompoundOperator, filters: Vec<SearchFilter>) -> SearchFilter {
    SearchFilter::Compound(CompoundFilter { operator, filters })
}

fn values() -> Vec<AttributeValue> {
    vec![
        AttributeValue::String(String::new()),
        AttributeValue::String("Z".into()),
        AttributeValue::String("a".into()),
        AttributeValue::String("é".into()),
        AttributeValue::String("e\u{301}".into()),
        AttributeValue::String("中文🦀".into()),
        AttributeValue::String("' OR true -- \" \\".into()),
        AttributeValue::String("1".into()),
        AttributeValue::Number(-1.5),
        AttributeValue::Number(-0.0),
        AttributeValue::Number(0.0),
        AttributeValue::Number(1.0),
        AttributeValue::Number(1.000_000_000_000_000_2),
        AttributeValue::Number(9_007_199_254_740_992.0),
        AttributeValue::Number(1e-100),
        AttributeValue::Number(1e100),
        AttributeValue::Boolean(false),
        AttributeValue::Boolean(true),
    ]
}

fn fixture_chunks(values: &[AttributeValue], escaped_key: &str) -> Vec<StoredChunk> {
    let mut attributes = vec![FileAttributes::new()];
    for value in values {
        attributes.push(FileAttributes::from([("value".into(), value.clone())]));
        attributes.push(FileAttributes::from([
            ("value".into(), value.clone()),
            (escaped_key.into(), value.clone()),
            ("other".into(), AttributeValue::Boolean(true)),
        ]));
    }
    attributes
        .into_iter()
        .enumerate()
        .map(|(chunk_index, attributes)| StoredChunk {
            file_id: "file-filter-fixture".into(),
            filename: "filters.txt".into(),
            chunk_index,
            text: "same candidate corpus".into(),
            embedding: None,
            attributes,
        })
        .collect()
}

fn fixture_filters(values: &[AttributeValue], escaped_key: &str) -> Vec<SearchFilter> {
    let mut filters = Vec::new();
    for key in ["value", escaped_key, "missing"] {
        for value in values {
            for operator in [
                ComparisonOperator::Eq,
                ComparisonOperator::Ne,
                ComparisonOperator::Gt,
                ComparisonOperator::Gte,
                ComparisonOperator::Lt,
                ComparisonOperator::Lte,
            ] {
                if matches!(value, AttributeValue::Boolean(_))
                    && !matches!(operator, ComparisonOperator::Eq | ComparisonOperator::Ne)
                {
                    continue;
                }
                filters.push(comparison(key, operator, FilterValue::Scalar(value.clone())));
            }
        }
        for members in [
            values.to_vec(),
            vec![values[1].clone()],
            vec![values[11].clone(), values[17].clone()],
        ] {
            for operator in [ComparisonOperator::In, ComparisonOperator::Nin] {
                filters.push(comparison(key, operator, FilterValue::List(members.clone())));
            }
        }
    }
    for index in (0..filters.len()).step_by(13) {
        let left = filters[index].clone();
        let right = comparison(
            "other",
            ComparisonOperator::Eq,
            FilterValue::Scalar(AttributeValue::Boolean(true)),
        );
        for operator in [CompoundOperator::And, CompoundOperator::Or] {
            filters.push(compound(operator, vec![left.clone(), right.clone()]));
            filters.push(compound(
                operator,
                vec![
                    left.clone(),
                    compound(CompoundOperator::Or, vec![left.clone(), right.clone()]),
                ],
            ));
        }
    }
    filters
}

#[tokio::test]
#[ignore = "requires isolated TEST_POSTGRES_URL; run serially"]
async fn postgres_pgvector_filter_pushdown_matches_rust_for_the_same_chunks() {
    let url = std::env::var("TEST_POSTGRES_URL").expect("TEST_POSTGRES_URL required");
    let pool = create_pool(Some(&url)).await.unwrap();
    let values = values();
    let escaped_key = "quote'\"\\Ω";
    let chunks = fixture_chunks(&values, escaped_key);
    let filters = fixture_filters(&values, escaped_key);
    for filter in &filters {
        SearchRequest {
            query: SearchQuery::Text("fixture".into()),
            filters: Some(filter.clone()),
            ..Default::default()
        }
        .validate()
        .unwrap();
        let expected: Vec<i64> = chunks
            .iter()
            .filter(|chunk| filter.matches(&chunk.attributes))
            .map(|chunk| i64::try_from(chunk.chunk_index).unwrap())
            .collect();
        let mut sql = CandidateSql::new("SELECT chunk_index FROM (VALUES ");
        for (index, chunk) in chunks.iter().enumerate() {
            if index > 0 {
                sql.push(", ");
            }
            sql.push(format!("({index}::bigint, "))
                .push_bind(serde_json::to_string(chunk).unwrap())
                .push("::text)");
        }
        sql.push(") AS chunks(chunk_index, data) WHERE ");
        push_filter(&mut sql, filter).unwrap();
        sql.push(" ORDER BY chunk_index");
        let mut query = sqlx::query_scalar::<_, i64>(&sql.text);
        for parameter in &sql.parameters {
            query = query.bind(parameter);
        }
        let actual = query.fetch_all(pool.as_ref()).await.unwrap();
        assert_eq!(actual, expected, "filter: {filter:?}");
    }
    assert!(
        filters.len() > 350,
        "exercise every operator, type, and nested compounds"
    );
}
