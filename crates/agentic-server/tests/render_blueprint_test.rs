use std::fs;
use std::path::PathBuf;

use serde::Deserialize;

use agentic_server::GATEWAY_DRAIN_TIMEOUT;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Blueprint {
    projects: Vec<Project>,
}

#[derive(Debug, Deserialize)]
struct Project {
    name: String,
    environments: Vec<Environment>,
}

#[derive(Debug, Deserialize)]
struct Environment {
    name: String,
    networking: Networking,
    services: Vec<Service>,
    databases: Vec<Database>,
}

#[derive(Debug, Deserialize)]
struct Networking {
    isolation: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Service {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    runtime: String,
    plan: String,
    dockerfile_path: String,
    docker_context: String,
    auto_deploy_trigger: String,
    health_check_path: Option<String>,
    max_shutdown_delay_seconds: u16,
    env_vars: Vec<EnvVar>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EnvVar {
    key: String,
    value: Option<String>,
    sync: Option<bool>,
    from_database: Option<DatabaseReference>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DatabaseReference {
    name: String,
    property: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Database {
    name: String,
    plan: String,
    ip_allow_list: Vec<serde_yml::Value>,
}

fn load_blueprint() -> Blueprint {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../render.yaml");
    let yaml = fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    serde_yml::from_str(&yaml).unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
}

fn env_var<'a>(service: &'a Service, key: &str) -> &'a EnvVar {
    service
        .env_vars
        .iter()
        .find(|env_var| env_var.key == key)
        .unwrap_or_else(|| panic!("{key} must be configured"))
}

#[test]
fn render_blueprint_wires_the_gateway_to_managed_postgres() {
    let blueprint = load_blueprint();
    let [project] = blueprint.projects.as_slice() else {
        panic!("blueprint must define exactly one project");
    };
    let [environment] = project.environments.as_slice() else {
        panic!("project must define exactly one environment");
    };
    let [service] = environment.services.as_slice() else {
        panic!("blueprint must define exactly one service");
    };
    let [database] = environment.databases.as_slice() else {
        panic!("blueprint must define exactly one database");
    };

    assert_eq!(project.name, "agentic-api");
    assert_eq!(environment.name, "production");
    assert_eq!(environment.networking.isolation, "enabled");

    assert_eq!(service.name, "agentic-api");
    assert_eq!(service.kind, "pserv");
    assert_eq!(service.runtime, "docker");
    assert_eq!(service.plan, "starter");
    assert_eq!(service.dockerfile_path, "./Dockerfile");
    assert_eq!(service.docker_context, ".");
    assert_eq!(service.auto_deploy_trigger, "checksPass");
    assert_eq!(
        service.health_check_path, None,
        "private services only support TCP health checks"
    );
    assert!(u64::from(service.max_shutdown_delay_seconds) > GATEWAY_DRAIN_TIMEOUT.as_secs());

    assert_eq!(env_var(service, "GATEWAY_PORT").value.as_deref(), Some("9000"));

    let llm_api_base = env_var(service, "LLM_API_BASE");
    assert_eq!(llm_api_base.sync, Some(false));
    assert_eq!(llm_api_base.value, None);

    let upstream_key = env_var(service, "OPENAI_API_KEY");
    assert_eq!(upstream_key.sync, Some(false));
    assert_eq!(upstream_key.value, None);

    let skip_llm_ready_check = env_var(service, "SKIP_LLM_READY_CHECK");
    assert_eq!(skip_llm_ready_check.sync, Some(false));
    assert_eq!(skip_llm_ready_check.value, None);

    let database_url = env_var(service, "DATABASE_URL")
        .from_database
        .as_ref()
        .expect("DATABASE_URL must reference managed Postgres");
    assert_eq!(database_url.name, database.name);
    assert_eq!(database_url.property, "connectionString");

    assert_eq!(database.name, "agentic-api-postgres");
    assert_eq!(database.plan, "basic-256mb");
    assert!(
        database.ip_allow_list.is_empty(),
        "managed Postgres must not accept public connections"
    );
}
