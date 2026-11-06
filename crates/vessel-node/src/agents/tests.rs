use vessel_proto::DeviceId;

use super::*;

fn agent_of(agents: &Agents, session: SessionId, scopes: Vec<Scope>) -> Principal {
    Principal::Agent {
        node: agents.node,
        session,
        scopes,
    }
}

fn auth(session: SessionId, secret: &AgentSecret) -> AgentAuth {
    AgentAuth {
        session,
        secret: secret.clone(),
    }
}

#[test]
fn a_credential_admits_its_session_only_until_it_ends() {
    let agents = Arc::new(Agents::new(NodeId::random()));
    let node_closing = CancellationToken::new();
    let id = SessionId::random();
    let secret = agents
        .reserve(id, &Principal::Owner, true)
        .unwrap()
        .unwrap();

    let (principal, registration) = agents.admit(&auth(id, &secret), &node_closing).unwrap();
    assert_eq!(
        principal,
        agent_of(&agents, id, vec![Scope::Read, Scope::Operate])
    );
    // Another session's id, or a guessed secret, is refused.
    assert!(
        agents
            .admit(&auth(SessionId::random(), &secret), &node_closing)
            .is_none()
    );
    let forged = AgentSecret::from_bytes([7; 32]);
    assert!(agents.admit(&auth(id, &forged), &node_closing).is_none());

    agents.retire(id);
    assert!(
        registration.closing().is_cancelled(),
        "the agent's connection stays open"
    );
    assert!(agents.admit(&auth(id, &secret), &node_closing).is_none());
    assert!(!node_closing.is_cancelled());
}

#[test]
fn grants_only_shrink() {
    let agents = Agents::new(NodeId::random());
    let viewer = Principal::Device {
        id: DeviceId::random(),
        scopes: vec![Scope::Operate, Scope::Admin],
    };
    let id = SessionId::random();
    let secret = agents.reserve(id, &viewer, true).unwrap().unwrap();
    let (principal, _registration) = Arc::new(agents)
        .admit(&auth(id, &secret), &CancellationToken::new())
        .unwrap();
    let Principal::Agent { scopes, .. } = principal else {
        panic!("{principal:?}");
    };
    // `admin` never; `read` not, since the device has none.
    assert_eq!(scopes, [Scope::Operate]);
}

#[test]
fn agents_start_agents_only_so_deep_and_own_only_so_many() {
    let agents = Agents::new(NodeId::random());
    let scopes = vec![Scope::Read, Scope::Operate];
    let mut parent = SessionId::random();
    let _none = agents.reserve(parent, &Principal::Owner, false).unwrap();
    for _level in 1..=MAX_DEPTH {
        let child = SessionId::random();
        let _none = agents
            .reserve(child, &agent_of(&agents, parent, scopes.clone()), false)
            .unwrap();
        parent = child;
    }
    let error = agents
        .reserve(
            SessionId::random(),
            &agent_of(&agents, parent, scopes.clone()),
            false,
        )
        .unwrap_err();
    assert!(
        matches!(error, ServiceError::AgentDepth { .. }),
        "{error:?}"
    );

    let busy = SessionId::random();
    let _none = agents.reserve(busy, &Principal::Owner, false).unwrap();
    let mut owned = Vec::new();
    for _ in 0..MAX_OWNED {
        let child = SessionId::random();
        let _none = agents
            .reserve(child, &agent_of(&agents, busy, scopes.clone()), false)
            .unwrap();
        owned.push(child);
    }
    let error = agents
        .reserve(
            SessionId::random(),
            &agent_of(&agents, busy, scopes.clone()),
            false,
        )
        .unwrap_err();
    assert!(
        matches!(error, ServiceError::AgentLimit { .. }),
        "{error:?}"
    );
    // An owned session that ends frees its place.
    agents.retire(owned.pop().unwrap());
    assert!(
        agents
            .reserve(SessionId::random(), &agent_of(&agents, busy, scopes), false)
            .is_ok()
    );
}

#[test]
fn a_move_keeps_the_credential_of_the_agent_that_runs() {
    let agents = Arc::new(Agents::new(NodeId::random()));
    let closing = CancellationToken::new();
    let id = SessionId::random();
    let old = agents
        .reserve(id, &Principal::Owner, true)
        .unwrap()
        .unwrap();

    let new = agents.begin_move(id).unwrap().unwrap();
    assert!(matches!(
        agents.begin_move(id),
        Err(ServiceError::MoveRunning(_))
    ));
    assert!(agents.admit(&auth(id, &old), &closing).is_some());
    assert!(agents.admit(&auth(id, &new), &closing).is_some());
    agents.end_move(id, true);
    assert!(agents.admit(&auth(id, &old), &closing).is_none());
    assert!(agents.admit(&auth(id, &new), &closing).is_some());

    let failed = agents.begin_move(id).unwrap().unwrap();
    agents.end_move(id, false);
    assert!(agents.admit(&auth(id, &failed), &closing).is_none());
    assert!(agents.admit(&auth(id, &new), &closing).is_some());
    // No move began: nothing changes.
    agents.end_move(id, true);
    assert!(agents.admit(&auth(id, &new), &closing).is_some());
}
