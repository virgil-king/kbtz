use kbtz_council::project::{Project, RepoConfig, Stakeholder};
use kbtz_council::step::{Step, StepPhase, Dispatch};

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
