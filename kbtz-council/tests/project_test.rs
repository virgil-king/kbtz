use kbtz_council::project::{Project, ProjectDir, RepoConfig, Stakeholder};
use kbtz_council::job::{Dispatch, Job, JobPhase, RepoRef};
use tempfile::TempDir;

#[test]
fn project_state_round_trip() {
    let project = Project {
        repos: vec![
            RepoConfig { name: "backend".into(), url: "/home/user/backend".into(), branch: None },
            RepoConfig { name: "frontend".into(), url: "/home/user/frontend".into(), branch: None },
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
fn job_state_round_trip() {
    let job = Job {
        id: "job-001".into(),
        phase: JobPhase::Dispatched,
        dispatch: Dispatch {
            prompt: "Add JWT auth middleware".into(),
            repos: vec![RepoRef { name: "backend".into(), branch: None }],
            files: vec![],
        },
        implementor: Some("agent".into()),
        agent_id: None,
        artifacts: vec![],
    };

    let json = serde_json::to_string_pretty(&job).unwrap();
    let parsed: Job = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.id, "job-001");
    assert!(matches!(parsed.phase, JobPhase::Dispatched));
}

#[test]
fn job_phases_serialize_as_lowercase() {
    let phases = vec![
        JobPhase::Dispatched,
        JobPhase::Running,
        JobPhase::Completed,
        JobPhase::Reviewing,
        JobPhase::Reviewed,
        JobPhase::Merged,
        JobPhase::Rework,
    ];
    for phase in phases {
        let json = serde_json::to_string(&phase).unwrap();
        let parsed: JobPhase = serde_json::from_str(&json).unwrap();
        assert_eq!(phase, parsed);
    }
}

#[test]
fn project_dir_init_creates_structure() {
    let tmp = TempDir::new().unwrap();
    let project = Project {
        repos: vec![RepoConfig { name: "myrepo".into(), url: "/tmp/myrepo".into(), branch: None }],
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
        repos: vec![RepoConfig { name: "myrepo".into(), url: "/tmp/myrepo".into(), branch: None }],
        stakeholders: vec![],
        goal_summary: "Test".into(),
    };

    let _dir = ProjectDir::init(tmp.path(), &project).unwrap();
    let loaded = ProjectDir::load(tmp.path()).unwrap();
    assert_eq!(loaded.state().project.goal_summary, "Test");
}

#[test]
fn state_tracks_jobs() {
    let tmp = TempDir::new().unwrap();
    let project = Project {
        repos: vec![],
        stakeholders: vec![],
        goal_summary: "Test".into(),
    };

    let mut dir = ProjectDir::init(tmp.path(), &project).unwrap();
    let job_id = dir.add_job(Dispatch {
        prompt: "Do the thing".into(),
        repos: vec![],
        files: vec![],
    }).unwrap();

    assert_eq!(job_id, "job-001");
    assert_eq!(dir.state().jobs.len(), 1);

    let loaded = ProjectDir::load(tmp.path()).unwrap();
    assert_eq!(loaded.state().jobs.len(), 1);
}

#[test]
fn recovery_rolls_back_inflight_phases() {
    let tmp = TempDir::new().unwrap();
    let project = Project {
        repos: vec![],
        stakeholders: vec![],
        goal_summary: "Test".into(),
    };

    let mut dir = ProjectDir::init(tmp.path(), &project).unwrap();

    // Add jobs in various in-flight phases
    dir.add_job(Dispatch {
        prompt: "job 1".into(),
        repos: vec![],
        files: vec![],
    }).unwrap();
    dir.add_job(Dispatch {
        prompt: "job 2".into(),
        repos: vec![],
        files: vec![],
    }).unwrap();
    dir.add_job(Dispatch {
        prompt: "job 3".into(),
        repos: vec![],
        files: vec![],
    }).unwrap();

    // Simulate phases that would exist if processes died mid-flight
    dir.state_mut().jobs[0].phase = JobPhase::Running;
    dir.state_mut().jobs[1].phase = JobPhase::Reviewing;
    dir.state_mut().jobs[2].phase = JobPhase::Reviewed; // should stay
    dir.persist().unwrap();

    // Reload and recover
    let dir2 = ProjectDir::load(tmp.path()).unwrap();
    let project_dir = std::sync::Arc::new(std::sync::Mutex::new(dir2));
    let mcp_config = tmp.path().join(".mcp.json");
    let mut orch = kbtz_council::orchestrator::Orchestrator::new(
        std::sync::Arc::clone(&project_dir),
        mcp_config,
    );
    orch.recover_from_state();

    let dir = project_dir.lock().unwrap();
    // Running -> Dispatched (so tick re-spawns with --resume)
    assert_eq!(dir.state().jobs[0].phase, JobPhase::Dispatched);
    // Reviewing -> Completed (so tick re-spawns stakeholders)
    assert_eq!(dir.state().jobs[1].phase, JobPhase::Completed);
    // Reviewed stays Reviewed (tick invokes leader)
    assert_eq!(dir.state().jobs[2].phase, JobPhase::Reviewed);
}
