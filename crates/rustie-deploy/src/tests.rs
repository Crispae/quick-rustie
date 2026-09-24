use crate::*;

const GOOD: &str = r#"
version: 1
cluster_id: rustie-cluster
storage:
  endpoint: https://s3.example.com
  region: eu2
  bucket: rustie-wiki
  access_key_id: ${TEST_DEPLOY_AK}
  secret_access_key: ${TEST_DEPLOY_SK}
  path_style: true
index:
  id: wiki5k
metastore_uri: postgres://rustie:${TEST_DEPLOY_PG}@10.0.0.5:5432/rustie
cachey:
  enabled: true
  url: http://10.0.0.9:9020
nodes:
  - { node_id: n1, host: 10.0.0.1, data_dir: /var/lib/rustie/n1 }
  - { node_id: n2, host: 10.0.0.2, data_dir: /var/lib/rustie/n2 }
serve:
  gateway_node: n1
"#;

fn set_env() {
    // SAFETY: tests in this crate only read these variables.
    unsafe {
        std::env::set_var("TEST_DEPLOY_AK", "ak");
        std::env::set_var("TEST_DEPLOY_SK", "sk");
        std::env::set_var("TEST_DEPLOY_PG", "pw");
    }
}

fn fields(r: &Report) -> Vec<String> {
    r.errors().map(|i| i.field.clone()).collect()
}

#[test]
fn good_config_has_no_errors() {
    set_env();
    let l = DeployConfig::parse(GOOD).unwrap();
    let r = validate(&l, &Role::All);
    assert!(!r.has_errors(), "{}", r.render("t"));
}

#[test]
fn empty_file_lists_every_required_field_with_a_fix() {
    let l = DeployConfig::parse("{}").unwrap();
    let r = validate(&l, &Role::All);
    let f = fields(&r);
    for want in [
        "version",
        "cluster_id",
        "storage.region",
        "storage.bucket",
        "storage.access_key_id",
        "storage.secret_access_key",
        "index.id",
        "metastore_uri",
        "nodes",
    ] {
        assert!(f.contains(&want.to_string()), "missing {want}: {f:?}");
    }
    assert!(r.errors().all(|i| !i.fix.is_empty()));
}

#[test]
fn unset_env_var_is_an_error_naming_the_variable() {
    let l = DeployConfig::parse(&GOOD.replace("TEST_DEPLOY_AK", "TEST_DEPLOY_UNSET_XYZ")).unwrap();
    let r = validate(&l, &Role::All);
    let e = r
        .errors()
        .find(|i| i.message.contains("TEST_DEPLOY_UNSET_XYZ"))
        .unwrap();
    assert_eq!(e.field, "storage.access_key_id");
}

#[test]
fn unknown_key_is_rejected() {
    let err = DeployConfig::parse("version: 1\nstorge: {}").err().unwrap();
    assert!(format!("{err}").contains("storge"));
}

#[test]
fn placeholders_are_errors() {
    set_env();
    let l = DeployConfig::parse(&GOOD.replace("rustie-wiki", "<your-bucket>")).unwrap();
    assert!(fields(&validate(&l, &Role::All)).contains(&"storage.bucket".to_string()));
}

#[test]
fn loopback_host_with_remote_peers_is_an_error() {
    set_env();
    let l = DeployConfig::parse(&GOOD.replace("host: 10.0.0.1", "host: 127.0.0.1")).unwrap();
    assert!(fields(&validate(&l, &Role::All)).contains(&"nodes[0].host".to_string()));
}

#[test]
fn colliding_ports_on_one_host_are_an_error() {
    set_env();
    let same = GOOD.replace("host: 10.0.0.2", "host: 10.0.0.1");
    let l = DeployConfig::parse(&same).unwrap();
    assert!(
        fields(&validate(&l, &Role::All))
            .iter()
            .any(|f| f.starts_with("nodes[1]."))
    );
}

#[test]
fn cachey_rejects_indexer_role_and_warns_with_split_cache() {
    set_env();
    let l = DeployConfig::parse(&GOOD.replace(
        "{ node_id: n1, host: 10.0.0.1, data_dir: /var/lib/rustie/n1 }",
        "{ node_id: n1, host: 10.0.0.1, data_dir: /d, services: [searcher, metastore, indexer] }",
    ))
    .unwrap();
    assert!(fields(&validate(&l, &Role::All)).contains(&"nodes[0].services".to_string()));

    let l = DeployConfig::parse(&format!("{GOOD}\nsplit_cache: {{ enabled: true }}")).unwrap();
    let r = validate(&l, &Role::All);
    assert!(
        r.issues
            .iter()
            .any(|i| i.severity == Severity::Warning && i.field.contains("split_cache"))
    );
}

#[test]
fn unknown_node_id_lists_the_valid_ones() {
    set_env();
    let l = DeployConfig::parse(GOOD).unwrap();
    let r = validate(
        &l,
        &Role::Node {
            node_id: "zzz".into(),
        },
    );
    let e = r.errors().find(|i| i.field == "--node-id").unwrap();
    assert!(e.fix.contains("n1") && e.fix.contains("n2"));
}

#[test]
fn non_aws_without_path_style_warns() {
    set_env();
    let l = DeployConfig::parse(&GOOD.replace("  path_style: true\n", "")).unwrap();
    let r = validate(&l, &Role::All);
    assert!(
        r.issues
            .iter()
            .any(|i| i.field == "storage.path_style" && i.severity == Severity::Warning)
    );
    assert!(l.config.effective_path_style());
    assert_eq!(l.config.effective_checksum(), "md5");
}

#[test]
fn literal_secrets_warn_and_are_never_rendered_in_the_report() {
    let l = DeployConfig::parse(&GOOD.replace("${TEST_DEPLOY_SK}", "topsecretvalue")).unwrap();
    let r = validate(&l, &Role::All);
    assert!(
        r.issues
            .iter()
            .any(|i| i.field == "storage.secret_access_key" && i.severity == Severity::Warning)
    );
    assert!(!r.render("t").contains("topsecretvalue"));
    assert!(!format!("{:?}", l.config).contains("topsecretvalue"));
}

#[test]
fn bad_gateway_node_is_an_error() {
    set_env();
    let l = DeployConfig::parse(&GOOD.replace("gateway_node: n1", "gateway_node: nope")).unwrap();
    assert!(fields(&validate(&l, &Role::Serve)).contains(&"serve.gateway_node".to_string()));
}

#[test]
fn render_lists_peers_and_explicit_s3_settings() {
    set_env();
    let l = DeployConfig::parse(GOOD).unwrap();
    let y = render_node_yaml(&l.config, "n1").unwrap();
    let v: serde_yaml::Value = serde_yaml::from_str(&y).unwrap();
    assert_eq!(v["node_id"], "n1");
    assert_eq!(v["peer_seeds"][0], "10.0.0.2:7282");
    assert_eq!(v["advertise_address"], "10.0.0.1");
    assert_eq!(v["storage"]["s3"]["region"], "eu2");
    assert_eq!(v["storage"]["s3"]["force_path_style_access"], true);
    assert_eq!(v["storage"]["s3"]["checksum_algorithm"], "md5");
    assert!(v["storage"]["s3"].get("flavor").is_none());
    assert_eq!(v["default_index_root_uri"], "s3://rustie-wiki/indexes");
    assert_eq!(
        gateway_endpoint(&l.config).as_deref(),
        Some("10.0.0.1:7281")
    );
}

#[test]
fn one_line_per_problem_field() {
    // Unset env var + empty value must not be reported twice for the same field.
    let l = DeployConfig::parse(&GOOD.replace("TEST_DEPLOY_AK", "TEST_DEPLOY_UNSET_XYZ")).unwrap();
    let r = validate(&l, &Role::All);
    assert_eq!(
        r.errors()
            .filter(|i| i.field == "storage.access_key_id")
            .count(),
        1
    );

    let l =
        DeployConfig::parse(&GOOD.replace("https://s3.example.com", "https://<your-s3-endpoint>"))
            .unwrap();
    let r = validate(&l, &Role::All);
    assert_eq!(
        r.errors().filter(|i| i.field == "storage.endpoint").count(),
        1
    );
}

#[test]
fn env_referenced_secrets_produce_no_literal_warnings() {
    set_env();
    let r = validate(&DeployConfig::parse(GOOD).unwrap(), &Role::All);
    let bad: Vec<_> = r
        .issues
        .iter()
        .filter(|i| i.message.contains("literally"))
        .collect();
    assert!(bad.is_empty(), "{}", r.render("t"));
}

#[test]
fn literal_postgres_password_warns_exactly_once() {
    set_env();
    let l = DeployConfig::parse(&GOOD.replace("${TEST_DEPLOY_PG}", "hunter2")).unwrap();
    let r = validate(&l, &Role::All);
    assert_eq!(
        r.issues
            .iter()
            .filter(|i| i.field == "metastore_uri" && i.message.contains("literally"))
            .count(),
        1
    );
    assert!(!r.render("t").contains("hunter2"));
}
