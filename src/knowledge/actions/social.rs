//! Peers, connections, messages and concept personas (`social.ts`).

use serde_json::{json, Value};

use super::params::{self, validate};
use super::persona_logic::{
    is_degenerate, looks_like_id, persona_cache_key, persona_instructions, tutor_persona,
};
use super::social_logic::{
    anonymous_handle_for, is_dormant, is_expired, is_node_key, moderation_from_score, rank_peers,
    PeerCandidate, PEER_TOKEN_TTL_MS, PEER_WINDOW_MS,
};
use super::social_sources::SocialSources;
use super::{action, Env};
use crate::json::number;
use crate::knowledge::store::{
    ConnectionOrigin, ConnectionPatch, KnowledgeError, SocialStore, StoreResult, StoredBadge,
    StoredConnection, StoredMessage, StoredPersona,
};
use crate::knowledge::ActionTable;

fn node_key_check(p: &serde_json::Map<String, Value>) -> StoreResult<()> {
    validate(p, &[params::text("nodeKey")])?;
    if !is_node_key(params::str_of(p, "nodeKey")) {
        return Err(KnowledgeError::validation(
            "nodeKey: nodeKey must be concept:<id> or path:<id>",
        ));
    }
    Ok(())
}

fn not_found() -> KnowledgeError {
    KnowledgeError::not_found("Connection not found")
}

fn load_authorized(
    store: &dyn SocialStore,
    id: &str,
    uid: &str,
    now: f64,
) -> StoreResult<StoredConnection> {
    let Some(mut conn) = store.get_connection(id)? else {
        return Err(not_found());
    };
    if !conn.participants.iter().any(|p| p == uid) {
        return Err(not_found());
    }
    if conn.status == "active" && is_dormant(conn.last_activity_at, now) {
        store.update_connection(
            id,
            ConnectionPatch {
                status: Some("archived".into()),
                archived_at: Some(now),
                ..ConnectionPatch::default()
            },
        )?;
        conn.status = "archived".into();
        conn.archived_at = Some(now);
    }
    Ok(conn)
}

fn require_active(conn: &StoredConnection) -> StoreResult<()> {
    if conn.status == "archived" {
        return Err(KnowledgeError::conflict("Connection is archived"));
    }
    Ok(())
}

fn badges_for(sources: &dyn SocialSources, canonical_id: &str, kind: &str) -> Vec<StoredBadge> {
    match sources.badge_labels(canonical_id, kind) {
        Ok(labels) if !labels.is_empty() => labels
            .into_iter()
            .enumerate()
            .map(|(i, label)| StoredBadge {
                id: format!("b{i}"),
                label,
                kind: "subtopic".into(),
            })
            .collect(),
        _ => vec![StoredBadge {
            id: "b0".into(),
            label: canonical_id.to_string(),
            kind: "subtopic".into(),
        }],
    }
}

/// Score a message against its topic and attach the verdict; scoring that fails lets the message through.
fn moderate(
    store: &dyn SocialStore,
    sources: &dyn SocialSources,
    connection: &str,
    message_id: &str,
    content: &str,
    badge_label: Option<&str>,
) {
    let mut moderation = moderation_from_score(1.0, None);
    if let Some(label) = badge_label {
        if let Ok((score, reason)) = sources.relevance(content, label) {
            moderation = moderation_from_score(score, reason);
        }
    }
    let _ = store.set_message_moderation(connection, message_id, moderation);
}

pub fn register(table: &mut ActionTable, env: &Env) {
    let sources = env.social.clone();
    let token = env.token.clone();

    table.insert("peers", {
        let token = token.clone();
        action(move |call, p, deps| {
            node_key_check(p)?;
            let key = params::str_of(p, "nodeKey");
            let now = deps.graphs.now_ms();
            let members: Vec<_> = deps
                .social
                .list_members_since(key, now - PEER_WINDOW_MS)?
                .into_iter()
                .filter(|m| m.uid != call.uid)
                .collect();
            if members.is_empty() {
                return Ok(json!({ "peers": [] }));
            }
            let caller = deps.vectors.get_canonical_concepts_by_owner(call.uid)?;
            let mut candidates = Vec::new();
            for member in members {
                let set = deps.vectors.get_canonical_concepts_by_owner(&member.uid)?;
                let peer_ref = token();
                deps.social.put_peer_token(
                    &peer_ref,
                    crate::knowledge::store::PeerTokenRecord {
                        peer_uid: member.uid.clone(),
                        issued_to: call.uid.to_string(),
                        node_key: key.to_string(),
                        expires_at: now + PEER_TOKEN_TTL_MS,
                    },
                )?;
                candidates.push(PeerCandidate {
                    peer_ref,
                    member,
                    canonical_names: set.canonical_names,
                    concepts_completed: set.count,
                });
            }
            let peers: Vec<Value> = rank_peers(&caller.canonical_names, candidates, now)
                .into_iter()
                .map(|peer| {
                    json!({
                        "peerRef": peer.peer_ref,
                        "handle": peer.handle,
                        "conceptsCompleted": peer.concepts_completed,
                        "convergencePct": number(peer.convergence_pct),
                        "activeNow": peer.active_now,
                    })
                })
                .collect();
            Ok(json!({ "peers": peers }))
        })
    });

    table.insert(
        "touchNode",
        action(|call, p, deps| {
            node_key_check(p)?;
            deps.social.touch_member(
                params::str_of(p, "nodeKey"),
                call.uid,
                deps.graphs.now_ms(),
            )?;
            Ok(json!({ "touched": true }))
        }),
    );

    table.insert("openConnection", {
        let sources = sources.clone();
        action(move |call, p, deps| {
            validate(
                p,
                &[
                    params::id("peerRef"),
                    params::pick("originKind", &["concept", "path"]),
                    params::id("canonicalId"),
                ],
            )?;
            let peer_ref = params::str_of(p, "peerRef");
            let origin_kind = params::str_of(p, "originKind");
            let canonical_id = params::str_of(p, "canonicalId");
            let now = deps.graphs.now_ms();
            let record = deps.social.get_peer_token(peer_ref)?;
            if let Some(r) = &record {
                if is_expired(r.expires_at, now) {
                    deps.social.delete_peer_token(peer_ref)?;
                }
            }
            let valid =
                record.filter(|r| !is_expired(r.expires_at, now) && r.issued_to == call.uid);
            let Some(record) = valid else {
                return Err(KnowledgeError::not_found(
                    "Peer reference is invalid or expired",
                ));
            };
            if record.peer_uid == call.uid {
                return Err(KnowledgeError::validation("Cannot connect to yourself"));
            }
            let badges = badges_for(sources.as_ref(), canonical_id, origin_kind);
            let first = badges[0].id.clone();
            let id = deps.social.create_connection(StoredConnection {
                caller_uid: call.uid.to_string(),
                peer_uid: record.peer_uid.clone(),
                participants: vec![call.uid.to_string(), record.peer_uid],
                origin: ConnectionOrigin {
                    kind: origin_kind.to_string(),
                    canonical_id: canonical_id.to_string(),
                },
                badges,
                active_badge_id: first,
                status: "active".into(),
                created_at: now,
                last_activity_at: now,
                archived_at: None,
            })?;
            Ok(json!({ "connectionId": id }))
        })
    });

    table.insert(
        "connection",
        action(|call, p, deps| {
            validate(p, &[params::id("connectionId")])?;
            let id = params::str_of(p, "connectionId");
            let conn = load_authorized(deps.social.as_ref(), id, call.uid, deps.graphs.now_ms())?;
            let rows = deps.social.list_messages(id)?;
            let messages: Vec<Value> = rows
                .iter()
                .map(|m| {
                    json!({
                        "id": m.id,
                        "fromViewer": m.message.sender_uid == call.uid,
                        "content": m.message.content,
                        "createdAt": number(m.message.created_at),
                        "flagged": m.message.moderation.as_ref().is_some_and(|x| x.flagged),
                        "moderationReason": m.message.moderation.as_ref().and_then(|x| x.reason.clone()).unwrap_or_default(),
                    })
                })
                .collect();
            let peer_uid = if conn.caller_uid == call.uid { &conn.peer_uid } else { &conn.caller_uid };
            Ok(json!({
                "status": conn.status,
                "peerHandle": anonymous_handle_for(peer_uid),
                "originKind": conn.origin.kind,
                "originLabel": conn.origin.canonical_id,
                "badges": conn.badges.iter().map(|b| json!({
                    "id": b.id, "label": b.label, "kind": b.kind, "active": b.id == conn.active_badge_id,
                })).collect::<Vec<_>>(),
                "messages": messages,
            }))
        }),
    );

    table.insert(
        "setActiveBadge",
        action(|call, p, deps| {
            validate(p, &[params::id("connectionId"), params::id("badgeId")])?;
            let id = params::str_of(p, "connectionId");
            let badge_id = params::str_of(p, "badgeId");
            let conn = load_authorized(deps.social.as_ref(), id, call.uid, deps.graphs.now_ms())?;
            require_active(&conn)?;
            if !conn.badges.iter().any(|b| b.id == badge_id) {
                return Err(KnowledgeError::not_found("Badge not found"));
            }
            deps.social.update_connection(
                id,
                ConnectionPatch {
                    active_badge_id: Some(badge_id.to_string()),
                    ..ConnectionPatch::default()
                },
            )?;
            Ok(json!({ "saved": true }))
        }),
    );

    table.insert("sendMessage", {
        let sources = sources.clone();
        action(move |call, p, deps| {
            validate(
                p,
                &[
                    params::id("connectionId"),
                    params::text("content"),
                    params::opt_id("activeBadgeId"),
                ],
            )?;
            let content = params::str_of(p, "content");
            if content.trim().is_empty() {
                return Err(KnowledgeError::validation("content: content required"));
            }
            let id = params::str_of(p, "connectionId");
            let requested = params::opt_str_of(p, "activeBadgeId");
            let now = deps.graphs.now_ms();
            let conn = load_authorized(deps.social.as_ref(), id, call.uid, now)?;
            require_active(&conn)?;
            let badge_id = requested.unwrap_or(&conn.active_badge_id).to_string();
            let badge = conn.badges.iter().find(|b| b.id == badge_id);
            if requested.is_some() && badge.is_none() {
                return Err(KnowledgeError::not_found("Badge not found"));
            }
            let message = StoredMessage {
                sender_uid: call.uid.to_string(),
                content: content.to_string(),
                created_at: now,
                active_badge_id: badge_id.clone(),
                moderation: None,
            };
            let message_id = deps.social.add_message(id, message)?;
            deps.social.update_connection(
                id,
                ConnectionPatch {
                    last_activity_at: Some(now),
                    ..ConnectionPatch::default()
                },
            )?;
            moderate(
                deps.social.as_ref(),
                sources.as_ref(),
                id,
                &message_id,
                content,
                badge.map(|b| b.label.as_str()),
            );
            Ok(json!({ "messageId": message_id }))
        })
    });

    table.insert("conceptPersona", {
        let sources = sources.clone();
        action(move |_call, p, deps| {
            validate(p, &[params::trimmed("conceptLabel"), params::opt_trimmed("context")])?;
            let label = params::str_of(p, "conceptLabel").trim().to_string();
            let context = params::opt_str_of(p, "context").map(str::trim);
            if looks_like_id(&label) {
                return Err(KnowledgeError::validation("conceptLabel must be a human-readable concept name"));
            }
            let key = persona_cache_key(&label, context);
            if let Some(cached) = deps.social.get_persona(&key)? {
                return Ok(json!({
                    "name": cached.name,
                    "description": cached.description,
                    "portraitUrl": cached.portrait_url,
                    "greeting": cached.greeting,
                    "instructions": persona_instructions(&cached.name, &cached.description, &cached.bio, &label, context),
                }));
            }
            let mut draft = sources.persona_draft(&label, None, context)?;
            if is_degenerate(&draft.name, &label) {
                draft = sources.persona_draft(&label, Some(&draft.name), context)?;
            }
            let degenerate = is_degenerate(&draft.name, &label);
            let wiki = if degenerate { Default::default() } else { sources.wiki_page(&draft.name) };
            let (name, description) = if degenerate {
                tutor_persona(&label)
            } else {
                (draft.name.clone(), wiki.description.clone().unwrap_or_else(|| draft.description.clone()))
            };
            let portrait_url = wiki.portrait_url.clone().unwrap_or_default();
            let bio = wiki.extract.clone().unwrap_or_default();
            let _ = deps.social.put_persona(
                &key,
                StoredPersona {
                    name: name.clone(),
                    description: description.clone(),
                    portrait_url: portrait_url.clone(),
                    greeting: draft.greeting.clone(),
                    bio: bio.clone(),
                    cached_at: deps.graphs.now_ms(),
                },
            );
            Ok(json!({
                "name": name,
                "description": description,
                "portraitUrl": portrait_url,
                "greeting": draft.greeting,
                "instructions": persona_instructions(&name, &description, &bio, &label, context),
            }))
        })
    });
}
