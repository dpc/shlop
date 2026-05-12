use tau_config::settings::ModelRegistry;
use tau_provider::storage::AuthStore;

#[test]
fn no_config_resolves_none() {
    let models = ModelRegistry::default();
    let mut auth = AuthStore::default();
    assert!(tau_provider::resolve(&"fake/model".into(), &models, &mut auth).is_none());
}
