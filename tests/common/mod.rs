/// Common test utilities and setup
pub fn setup_test_env() {
    // Initialize logging for tests
    let _ = env_logger::builder().is_test(true).try_init();
}
