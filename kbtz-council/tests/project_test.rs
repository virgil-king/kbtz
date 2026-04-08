use kbtz_council::project::{Project, ProjectDir, RepoConfig, Stakeholder};
use kbtz_council::step::{Dispatch, Step, StepPhase};
use tempfile::TempDir;

#[test]
fn project_state_round_trip() {
    let project = Project {
        repos: vec![
            RepoConfig { name: "backend".into(), url: "/home/user/backend".into() },
            RepoConfig { name: "frontend".into(), url: "/home/user/frontend".into() },
        ],
        stakeholders: vec![
            Stakeholder { name: "security".into(), persona: "Review for auth and injection vulnerabilities.".into() },
            Stakeholder { name: "api-design".into(), persona: "Review for REST conventions and backwards compatibility.".into() },
        ],
        goal_summary: "Add user authentication to the API".into(),
    };

    let json = serde_json::to_string_pretty(&project).unwrap();
    let parsed: Project = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.repos.len(), 2);
    assert_eq!(parsed.stakeholders[0].name, "security");
    assert_eq!(parsed.goal_summary, "Add user authentication to the API");
}

#[test]
fn step_state_round_trip() {
    let step = Step {
        id: "step-001".into(),
        phase: StepPhase::Dispatched,
        dispatch: Dispatch {
            prompt: "Add JWT auth middleware".into(),
            repos: vec!["backend".into()],
            files: vec![],
        },
        summary: None,
        feedback: vec![],
        decision: None,
    };

    let json = serde_json::to_string_pretty(&step).unwrap();
    let parsed: Step = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.id, "step-001");
    assert!(matches!(parsed.phase, StepPhase::Dispatched));
}

#[test]
fn step_phases_serialize_as_lowercase() {
    let phases = vec![
        StepPhase::Dispatched,
        StepPhase::Running,
        StepPhase::Completed,
        StepPhase::Reviewing,
        StepPhase::Reviewed,
        StepPhase::Merged,
        StepPhase::Rework,
    ];
    for phase in phases {
        let json = serde_json::to_string(&phase).unwrap();
        let parsed: StepPhase = serde_json::from_str(&json).unwrap();
        assert_eq!(phase, parsed);
    }
}

#[test]
fn project_dir_init_creates_structure() {
    let tmp = TempDir::new().unwrap();
    let project = Project {
        repos: vec![RepoConfig { name: "myrepo".into(), url: "/tmp/myrepo".into() }],
        stakeholders: vec![
            Stakeholder { name: "security".into(), persona: "Check auth".into() },
        ],
        goal_summary: "Test project".into(),
    };

    let dir = ProjectDir::init(tmp.path(), &project).unwrap();

    assert!(dir.root().join("state.json").exists());
    assert!(dir.root().join("repos").is_dir());
    assert!(dir.root().join("steps").is_dir());
    assert!(dir.root().join("sessions").is_dir());
    assert!(dir.root().join("claude-sessions").is_dir());
}

#[test]
fn project_dir_load_reads_state() {
    let tmp = TempDir::new().unwrap();
    let project = Project {
        repos: vec![RepoConfig { name: "myrepo".into(), url: "/tmp/myrepo".into() }],
        stakeholders: vec![],
        goal_summary: "Test".into(),
    };

    let _dir = ProjectDir::init(tmp.path(), &project).unwrap();
    let loaded = ProjectDir::load(tmp.path()).unwrap();
    assert_eq!(loaded.state().project.goal_summary, "Test");
}

#[test]
fn state_tracks_steps() {
    let tmp = TempDir::new().unwrap();
    let project = Project {
        repos: vec![],
        stakeholders: vec![],
        goal_summary: "Test".into(),
    };

    let mut dir = ProjectDir::init(tmp.path(), &project).unwrap();
    let step_id = dir.add_step(Dispatch {
        prompt: "Do the thing".into(),
        repos: vec![],
        files: vec![],
    }).unwrap();

    assert_eq!(step_id, "step-001");
    assert_eq!(dir.state().steps.len(), 1);

    let loaded = ProjectDir::load(tmp.path()).unwrap();
    assert_eq!(loaded.state().steps.len(), 1);
}
