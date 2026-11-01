use std::{env, fs, path::Path};

#[test]
fn destructive_gate_requires_explicit_approved_root_and_deterministic_seeds() {
    let Ok(root) = env::var("AMTE_RELIABILITY_ROOT") else {
        return;
    };
    assert!(root.starts_with("/tmp/opencode/"));
    let seeds = env::var("AMTE_RELIABILITY_SEEDS").expect("gate must log seeds");
    assert!(seeds.split(',').count() >= 4);
    let project = Path::new(&root).join("project");
    let config = Path::new(&root).join("xdg-config");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&config).unwrap();
    fs::write(project.join("large.md"), "x".repeat(400 * 1024)).unwrap();
    assert_eq!(
        fs::metadata(project.join("large.md")).unwrap().len(),
        400 * 1024
    );
}
