use kbtz_council::global::{GlobalState, ProjectStatus};
use kbtz_council::project::Project;
use tempfile::TempDir;

fn empty_project() -> Project {
    Project {
        repos: vec![],
        stakeholders: vec![],
        goal_summary: String::new(),
    }
}

#[test]
fn open_creates_directory_structure() {
    let tmp = TempDir::new().unwrap();
    let _global = GlobalState::open(tmp.path()).unwrap();

    assert!(tmp.path().join("projects").is_dir());
    assert!(tmp.path().join("archive").is_dir());
    assert!(tmp.path().join("pool").is_dir());
    assert!(tmp.path().join("index.json").exists());
}

#[test]
fn open_is_idempotent() {
    let tmp = TempDir::new().unwrap();
    let mut g = GlobalState::open(tmp.path()).unwrap();
    g.create_project("test", "a goal", &empty_project()).unwrap();

    // Reopen — should load the existing index, not overwrite it.
    let g2 = GlobalState::open(tmp.path()).unwrap();
    assert_eq!(g2.index().projects.len(), 1);
    assert_eq!(g2.index().projects[0].name, "test");
}

#[test]
fn create_project_registers_in_index() {
    let tmp = TempDir::new().unwrap();
    let mut global = GlobalState::open(tmp.path()).unwrap();

    let dir = global.create_project("my-proj", "Fix the auth bug", &empty_project()).unwrap();

    assert!(dir.root().join("state.json").exists());

    let entries = global.list_projects(None);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "my-proj");
    assert_eq!(entries[0].goal, "Fix the auth bug");
    assert_eq!(entries[0].status, ProjectStatus::Active);
    assert_eq!(entries[0].path, "projects/my-proj");
}

#[test]
fn create_project_rejects_duplicates() {
    let tmp = TempDir::new().unwrap();
    let mut global = GlobalState::open(tmp.path()).unwrap();

    global.create_project("dup", "goal", &empty_project()).unwrap();
    let err = global.create_project("dup", "goal", &empty_project()).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
}

#[test]
fn load_project_round_trips() {
    let tmp = TempDir::new().unwrap();
    let mut global = GlobalState::open(tmp.path()).unwrap();

    global.create_project("rt", "round trip test", &empty_project()).unwrap();
    let loaded = global.load_project("rt").unwrap();
    assert_eq!(loaded.state().project.goal_summary, "");
}

#[test]
fn load_project_not_found() {
    let tmp = TempDir::new().unwrap();
    let global = GlobalState::open(tmp.path()).unwrap();
    let err = global.load_project("nope").unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn list_projects_filters_by_status() {
    let tmp = TempDir::new().unwrap();
    let mut global = GlobalState::open(tmp.path()).unwrap();

    global.create_project("a", "goal a", &empty_project()).unwrap();
    global.create_project("b", "goal b", &empty_project()).unwrap();
    global.set_status("b", ProjectStatus::Archived).unwrap();

    let active = global.list_projects(Some(ProjectStatus::Active));
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].name, "a");

    let archived = global.list_projects(Some(ProjectStatus::Archived));
    assert_eq!(archived.len(), 1);
    assert_eq!(archived[0].name, "b");

    let all = global.list_projects(None);
    assert_eq!(all.len(), 2);
}

#[test]
fn set_status_moves_directory() {
    let tmp = TempDir::new().unwrap();
    let mut global = GlobalState::open(tmp.path()).unwrap();

    global.create_project("mv", "goal", &empty_project()).unwrap();
    assert!(tmp.path().join("projects/mv/state.json").exists());

    global.set_status("mv", ProjectStatus::Archived).unwrap();
    assert!(!tmp.path().join("projects/mv").exists());
    assert!(tmp.path().join("archive/mv/state.json").exists());

    // Index entry updated
    let entry = global.index().projects.iter().find(|e| e.name == "mv").unwrap();
    assert_eq!(entry.status, ProjectStatus::Archived);
    assert_eq!(entry.path, "archive/mv");
}

#[test]
fn set_status_resume_from_archive() {
    let tmp = TempDir::new().unwrap();
    let mut global = GlobalState::open(tmp.path()).unwrap();

    global.create_project("res", "goal", &empty_project()).unwrap();
    global.set_status("res", ProjectStatus::Archived).unwrap();
    global.set_status("res", ProjectStatus::Active).unwrap();

    assert!(tmp.path().join("projects/res/state.json").exists());
    assert!(!tmp.path().join("archive/res").exists());

    // Can still load
    let _ = global.load_project("res").unwrap();
}

#[test]
fn set_status_paused_stays_in_projects() {
    let tmp = TempDir::new().unwrap();
    let mut global = GlobalState::open(tmp.path()).unwrap();

    global.create_project("pau", "goal", &empty_project()).unwrap();
    global.set_status("pau", ProjectStatus::Paused).unwrap();

    // Paused projects stay in projects/ (not moved to archive/)
    assert!(tmp.path().join("projects/pau/state.json").exists());
    assert!(!tmp.path().join("archive/pau").exists());

    let entry = global.index().projects.iter().find(|e| e.name == "pau").unwrap();
    assert_eq!(entry.status, ProjectStatus::Paused);
    assert_eq!(entry.path, "projects/pau");

    // Paused -> Archived moves to archive/
    global.set_status("pau", ProjectStatus::Archived).unwrap();
    assert!(!tmp.path().join("projects/pau").exists());
    assert!(tmp.path().join("archive/pau/state.json").exists());
}

#[test]
fn set_status_noop_for_same_status() {
    let tmp = TempDir::new().unwrap();
    let mut global = GlobalState::open(tmp.path()).unwrap();

    global.create_project("nop", "goal", &empty_project()).unwrap();
    global.set_status("nop", ProjectStatus::Active).unwrap(); // no-op
    assert!(tmp.path().join("projects/nop/state.json").exists());
}

#[test]
fn set_status_not_found() {
    let tmp = TempDir::new().unwrap();
    let mut global = GlobalState::open(tmp.path()).unwrap();
    let err = global.set_status("ghost", ProjectStatus::Paused).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn project_path_resolves_correctly() {
    let tmp = TempDir::new().unwrap();
    let mut global = GlobalState::open(tmp.path()).unwrap();

    global.create_project("pp", "goal", &empty_project()).unwrap();
    assert_eq!(global.project_path("pp").unwrap(), tmp.path().join("projects/pp"));

    global.set_status("pp", ProjectStatus::Archived).unwrap();
    assert_eq!(global.project_path("pp").unwrap(), tmp.path().join("archive/pp"));
}

#[test]
fn index_persists_across_reopen() {
    let tmp = TempDir::new().unwrap();

    {
        let mut global = GlobalState::open(tmp.path()).unwrap();
        global.create_project("p1", "goal 1", &empty_project()).unwrap();
        global.create_project("p2", "goal 2", &empty_project()).unwrap();
        global.set_status("p2", ProjectStatus::Archived).unwrap();
    }

    let global = GlobalState::open(tmp.path()).unwrap();
    assert_eq!(global.index().projects.len(), 2);

    let p1 = global.index().projects.iter().find(|e| e.name == "p1").unwrap();
    assert_eq!(p1.status, ProjectStatus::Active);

    let p2 = global.index().projects.iter().find(|e| e.name == "p2").unwrap();
    assert_eq!(p2.status, ProjectStatus::Archived);
    assert_eq!(p2.path, "archive/p2");
}
