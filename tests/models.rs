use std::fs;
use std::os::unix::fs::PermissionsExt;

use pair_watcher::credentials::{CredError, Keys};
use pair_watcher::home::Home;
use pair_watcher::models::{self, Choice, ChoiceError, Table, Unpriced, Usage, Vendor};
use tempfile::TempDir;

fn home() -> (TempDir, Home) {
    let dir = TempDir::new().unwrap();
    let home = Home::new(dir.path().join(".pair-watcher"));
    home.ensure_layout().unwrap();
    (dir, home)
}

const BOTH: &[Vendor] = &[Vendor::Anthropic, Vendor::Openai];

#[test]
fn embedded_table_passes_its_own_schema_check() {
    let t = Table::embedded();
    t.check().unwrap();
    assert_eq!(t.defaults[&Vendor::Anthropic], "claude-sonnet-5-5");
    assert_eq!(t.defaults[&Vendor::Openai], "gpt-6.1-sol");
}

#[test]
fn cost_is_tokens_times_price_per_million() {
    let t = Table::embedded();
    let sonnet = t.price("claude-sonnet-5-5").unwrap();
    let u = Usage { input: 1_000_000, cached: 1_000_000, cache_write: 1_000_000, output: 1_000_000 };
    assert!((sonnet.cost(&u) - (2.0 + 0.2 + 2.5 + 10.0)).abs() < 1e-9);
    // No cache-write price: writes bill as plain input.
    let sol = t.price("gpt-6.1-sol").unwrap();
    let u = Usage { cache_write: 1_000_000, ..Usage::default() };
    assert!((sol.cost(&u) - 2.0).abs() < 1e-9);
}

/// V47: an unpriced model refuses to start.
#[test]
fn unpriced_model_is_refused() {
    let t = Table::embedded();
    assert_eq!(t.price("gpt-9"), Err(Unpriced("gpt-9".into())));
    assert_eq!(models::choose(&t, "gpt-9", None, BOTH), Err(ChoiceError::Unpriced(Unpriced("gpt-9".into()))));
}

#[test]
fn user_table_merges_over_embedded() {
    let (_d, home) = home();
    fs::write(
        home.models_file(),
        "[models.\"gpt-6.1-sol\"]\nvendor = \"openai\"\ninput = 3.0\ncached = 0.3\noutput = 12.0\n\n\
         [models.\"local-x\"]\nvendor = \"anthropic\"\ninput = 1.0\ncached = 0.1\noutput = 2.0\n",
    )
    .unwrap();
    let t = Table::load(&home).unwrap();
    assert_eq!(t.price("gpt-6.1-sol").unwrap().input, 3.0);
    assert!(t.price("local-x").is_ok());
    assert!(t.price("claude-opus-5-5").is_ok(), "embedded entries stay");
}

#[test]
fn user_table_with_zero_price_or_unpriced_default_is_rejected() {
    let (_d, home) = home();
    fs::write(
        home.models_file(),
        "[models.\"gpt-6-luna\"]\nvendor = \"openai\"\ninput = 0\ncached = 0.01\noutput = 0.5\n",
    )
    .unwrap();
    let e = Table::load(&home).unwrap_err();
    assert!(e.contains("gpt-6-luna") && e.contains("above zero"), "{e}");

    fs::write(home.models_file(), "[defaults]\nopenai = \"gpt-9\"\n").unwrap();
    let e = Table::load(&home).unwrap_err();
    assert!(e.contains("gpt-9") && e.contains("no price"), "{e}");

    fs::write(home.models_file(), "[defaults]\nopenai = \"claude-haiku-4-5\"\n").unwrap();
    assert!(Table::load(&home).unwrap_err().contains("another vendor"));
}

/// V46: mid-tier cross-vendor among configured backends.
#[test]
fn auto_picks_cross_vendor_mid_tier() {
    let t = Table::embedded();
    let pick = |worker, configured| models::choose(&t, "auto", worker, configured);
    let sol = Choice { model: "gpt-6.1-sol".into(), vendor: Vendor::Openai, same_vendor: false };
    let sonnet = Choice { model: "claude-sonnet-5-5".into(), vendor: Vendor::Anthropic, same_vendor: false };

    assert_eq!(pick(Some(Vendor::Anthropic), BOTH), Ok(sol.clone()), "Claude Worker → gpt-6.1-sol");
    assert_eq!(pick(Some(Vendor::Openai), BOTH), Ok(sonnet.clone()), "Codex Worker → Sonnet 5.5");
    assert_eq!(pick(Some(Vendor::Openai), &[Vendor::Openai]), Ok(Choice { same_vendor: true, ..sol }));
    assert!(!pick(Some(Vendor::Anthropic), &[Vendor::Openai]).unwrap().same_vendor);
    // Other provider (pi on another vendor): either differs.
    assert!(!pick(None, BOTH).unwrap().same_vendor);
    assert_eq!(pick(Some(Vendor::Anthropic), &[]), Err(ChoiceError::NoBackend));
}

#[test]
fn explicit_model_needs_its_vendors_key() {
    let t = Table::embedded();
    assert_eq!(
        models::choose(&t, "claude-opus-5-5", None, &[Vendor::Openai]),
        Err(ChoiceError::NoKey(Vendor::Anthropic))
    );
    let c = models::choose(&t, "claude-opus-5-5", Some(Vendor::Anthropic), BOTH).unwrap();
    assert_eq!((c.vendor, c.same_vendor), (Vendor::Anthropic, false));
}

#[test]
fn pi_provider_names_map_to_vendors() {
    assert_eq!(Vendor::of_provider("anthropic"), Some(Vendor::Anthropic));
    assert_eq!(Vendor::of_provider("openai-codex"), Some(Vendor::Openai));
    assert_eq!(Vendor::of_provider("google"), None);
}

const FETCHED: &str = "version = 1\n[defaults]\nanthropic = \"claude-sonnet-5-5\"\nopenai = \"gpt-6.1-sol\"\n\
    [models.\"claude-sonnet-5-5\"]\nvendor = \"anthropic\"\ninput = 2.0\ncached = 0.2\ncache_write = 2.5\noutput = 10.0\n\
    [models.\"gpt-6.1-sol\"]\nvendor = \"openai\"\ninput = 2.5\ncached = 0.1\noutput = 10.0\n\
    [models.\"gpt-7-mini\"]\nvendor = \"openai\"\ninput = 0.5\ncached = 0.05\noutput = 2.0\n";

/// V62: schema check, diff, apply, rollback restores previous.
#[test]
fn update_diffs_applies_and_rolls_back() {
    let (_d, home) = home();
    let (new, diff) = models::prepare_update(&home, FETCHED).unwrap();
    assert!(diff.contains(&"~ gpt-6.1-sol input: 2 → 2.5".to_owned()), "{diff:?}");
    assert!(diff.iter().any(|l| l.starts_with("+ gpt-7-mini (openai)")), "{diff:?}");
    assert_eq!(new.price("gpt-6.1-sol").unwrap().input, 2.5);

    assert!(!models::rollback(&home).unwrap(), "nothing to roll back yet");
    models::apply_update(&home, FETCHED).unwrap();
    assert_eq!(Table::load(&home).unwrap().price("gpt-6.1-sol").unwrap().input, 2.5);
    assert!(models::prepare_update(&home, FETCHED).unwrap().1.is_empty(), "second update: no change");

    assert!(models::rollback(&home).unwrap());
    assert!(!home.models_file().exists(), "no user file before the update");
    assert_eq!(Table::load(&home).unwrap(), Table::embedded());
}

#[test]
fn rollback_restores_a_previous_user_file() {
    let (_d, home) = home();
    let mine = "[models.\"local-x\"]\nvendor = \"openai\"\ninput = 1.0\ncached = 0.1\noutput = 2.0\n";
    fs::write(home.models_file(), mine).unwrap();
    let (_, diff) = models::prepare_update(&home, FETCHED).unwrap();
    assert!(diff.contains(&"- local-x".to_owned()), "{diff:?}");
    models::apply_update(&home, FETCHED).unwrap();
    models::rollback(&home).unwrap();
    assert_eq!(fs::read_to_string(home.models_file()).unwrap(), mine);
}

#[test]
fn update_refuses_bad_tables() {
    let (_d, home) = home();
    let zero = FETCHED.replace("input = 0.5", "input = 0");
    assert!(models::prepare_update(&home, &zero).unwrap_err().contains("above zero"));
    let no_version = FETCHED.replace("version = 1\n", "");
    assert!(models::prepare_update(&home, &no_version).unwrap_err().contains("version"));
    let future = FETCHED.replace("version = 1", "version = 2");
    assert!(models::prepare_update(&home, &future).unwrap_err().contains("version"));
    let unpriced_default = FETCHED.replace("openai = \"gpt-6.1-sol\"", "openai = \"gpt-8\"");
    assert!(models::prepare_update(&home, &unpriced_default).unwrap_err().contains("no price"));
    assert!(models::prepare_update(&home, "not = [toml").is_err());
    assert!(!home.models_file().exists(), "nothing written");
}

/// I18: env first, then `credentials`; I2: refused when group/world-readable.
#[test]
fn keys_come_from_env_then_credentials_file() {
    let (_d, home) = home();
    let file = home.credentials_file();
    fs::write(&file, "ANTHROPIC_API_KEY = \"from-file\"\nOPENAI_API_KEY = \"file-openai\"\n").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();

    let env = |k: &str| (k == "OPENAI_API_KEY").then(|| "from-env".to_owned());
    let keys = Keys::load(&home, &env);
    assert_eq!(keys.get(Vendor::Anthropic), Some("from-file"));
    assert_eq!(keys.get(Vendor::Openai), Some("from-env"));
    assert_eq!(keys.configured(), BOTH);
    assert_eq!(keys.problem, None);
}

#[test]
fn readable_credentials_file_is_refused_but_env_still_works() {
    let (_d, home) = home();
    let file = home.credentials_file();
    fs::write(&file, "ANTHROPIC_API_KEY = \"leaky\"\n").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();

    let env = |k: &str| (k == "OPENAI_API_KEY").then(|| "from-env".to_owned());
    let keys = Keys::load(&home, &env);
    assert_eq!(keys.get(Vendor::Anthropic), None, "refused file is not read");
    assert_eq!(keys.configured(), vec![Vendor::Openai]);
    assert_eq!(keys.problem, Some(CredError::Exposed(file)));
    assert!(!keys.problem.unwrap().to_string().contains("leaky"));
}
