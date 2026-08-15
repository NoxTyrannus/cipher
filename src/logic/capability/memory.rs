use crate::data::thought_store::ThoughtStore;
use crate::data::triviumdb::TriviumDb;
use serde_json::Value;
use std::sync::Arc;

pub const DEFAULT_MEMORY_LIMIT: usize = 100;
const MAX_WRITE_ENTRIES: usize = 100;
const MAX_ATTENTION_ENTRIES: usize = 2000;
const ZERO_DIM: usize = crate::data::triviumdb::DEFAULT_DIM;

fn err(msg: impl Into<String>) -> Result<Value, String> {
    Ok(serde_json::json!({ "success": false, "error": msg.into() }))
}

fn ok_value(value: Value) -> Result<Value, String> {
    Ok(value)
}

fn arg_str<'a>(args: &'a Value, field: &str, op: &str) -> Result<&'a str, String> {
    args.get(field)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("{op}: missing string field '{field}'"))
}

fn arg_array<'a>(args: &'a Value, field: &str, op: &str) -> Result<&'a Vec<Value>, String> {
    args.get(field)
        .and_then(|v| v.as_array())
        .ok_or_else(|| format!("{op}: missing array field '{field}'"))
}

fn arg_limit(args: &Value) -> usize {
    args.get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_MEMORY_LIMIT as u64)
        .clamp(1, 1000) as usize
}

fn memory_type_match(payload: &Value, memory_type: &str) -> bool {
    match payload.get("_memory_type").and_then(|v| v.as_str()) {
        Some("cognitive_edge") => memory_type == "cognitive",
        Some(t) => t == memory_type,
        None => false,
    }
}

fn payload_text(payload: &Value) -> String {
    payload.to_string().to_lowercase()
}

pub fn memory_list(db: &TriviumDb, args: &Value) -> Result<Value, String> {
    let memory_type = arg_str(args, "memory_type", "memory.list")?;
    let limit = arg_limit(args);
    let ids = db.db().get_all_ids();
    let mut items = Vec::new();
    for id in ids.into_iter().rev() {
        if items.len() >= limit {
            break;
        }
        let payload = match db.db().get_payload(id) {
            Some(p) => p,
            None => continue,
        };
        if !memory_type_match(&payload, memory_type) {
            continue;
        }
        items.push(serde_json::json!({"id": id, "payload": payload}));
    }
    ok_value(serde_json::json!({"items": items, "count": items.len()}))
}

pub fn memory_retrieve(db: &TriviumDb, args: &Value) -> Result<Value, String> {
    let memory_type = arg_str(args, "memory_type", "memory.retrieve")?;
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty());
    let limit = arg_limit(args);
    let ids = db.db().get_all_ids();
    let mut items = Vec::new();
    for id in ids.into_iter().rev() {
        if items.len() >= limit {
            break;
        }
        let payload = match db.db().get_payload(id) {
            Some(p) => p,
            None => continue,
        };
        if !memory_type_match(&payload, memory_type) {
            continue;
        }
        if let Some(q) = &query {
            if !payload_text(&payload).contains(q.as_str()) {
                continue;
            }
        }
        items.push(serde_json::json!({"id": id, "payload": payload}));
    }
    ok_value(serde_json::json!({"items": items, "count": items.len()}))
}

pub fn memory_delete(db: &mut TriviumDb, args: &Value) -> Result<Value, String> {
    let memory_type = arg_str(args, "memory_type", "memory.delete")?;
    let target_id = args.get("id").and_then(|v| v.as_u64());
    let focus = args
        .get("focus")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty());
    if target_id.is_none() && focus.is_none() {
        return err("memory.delete: one of 'id' or 'focus' is required");
    }
    let mut removed = 0usize;
    for id in db.db().get_all_ids() {
        let payload = match db.db().get_payload(id) {
            Some(p) => p,
            None => continue,
        };
        if !memory_type_match(&payload, memory_type) {
            continue;
        }
        let id_match = target_id == Some(id);
        let focus_match = focus.is_some_and(|f| {
            payload
                .get("focus")
                .and_then(|v| v.as_str())
                .is_some_and(|p| p == f)
        });
        if id_match || focus_match {
            db.db_mut()
                .delete(id)
                .map_err(|e| format!("memory.delete: {e}"))?;
            removed += 1;
        }
    }
    db.flush()
        .map_err(|e| format!("memory.delete flush: {e}"))?;
    ok_value(serde_json::json!({"removed": removed}))
}

pub fn memory_attention_write(db: &mut TriviumDb, args: &Value) -> Result<Value, String> {
    let entries = arg_array(args, "entries", "memory.attention.write")?;
    if entries.is_empty() || entries.len() > MAX_WRITE_ENTRIES {
        return err(format!(
            "memory.attention.write: entries must contain 1..={MAX_WRITE_ENTRIES} items"
        ));
    }
    let existing = db
        .db()
        .get_all_ids()
        .iter()
        .filter(|id| {
            db.db()
                .get_payload(**id)
                .and_then(|p| p.get("_memory_type").cloned())
                .and_then(|v| v.as_str().map(|s| s.to_string()))
                .as_deref()
                == Some("attention")
        })
        .count();
    if existing + entries.len() > MAX_ATTENTION_ENTRIES {
        return err(format!(
            "memory.attention.write: attention cap reached ({existing}/{MAX_ATTENTION_ENTRIES}); retire old entries first"
        ));
    }
    let mut ids = Vec::new();
    for entry in entries {
        let focus = entry
            .get("focus")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "memory.attention.write: entry missing 'focus'".to_string())?;
        let content = entry
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "memory.attention.write: entry missing 'content'".to_string())?;
        if focus.trim().is_empty() || content.trim().is_empty() {
            return err("memory.attention.write: focus/content must be non-empty");
        }
        let mut payload = serde_json::Map::new();
        payload.insert("focus".into(), Value::String(focus.to_string()));
        payload.insert("content".into(), Value::String(content.to_string()));
        payload.insert("_memory_type".into(), Value::String("attention".into()));
        if let Some(refs) = entry.get("source_refs").and_then(|v| v.as_array()) {
            if refs.iter().all(|v| v.is_string()) {
                payload.insert(
                    "source_refs".into(),
                    entry.get("source_refs").cloned().unwrap(),
                );
            }
        }
        let vector = vec![0.0_f32; ZERO_DIM];
        let id = db
            .db_mut()
            .insert(&vector, Value::Object(payload))
            .map_err(|e| format!("memory.attention.write insert: {e}"))?;
        ids.push(id);
    }
    db.flush()
        .map_err(|e| format!("memory.attention.write flush: {e}"))?;
    ok_value(serde_json::json!({"written": ids.len(), "ids": ids}))
}

pub fn memory_attention_retire(db: &mut TriviumDb, args: &Value) -> Result<Value, String> {
    let focus = arg_array(args, "focus", "memory.attention.retire")?;
    let focus: Vec<String> = focus
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();
    if focus.is_empty() {
        return err("memory.attention.retire: focus must be a non-empty string array");
    }
    let mut removed = 0usize;
    let mut retired = Vec::new();
    for id in db.db().get_all_ids() {
        let Some(payload) = db.db().get_payload(id) else {
            continue;
        };
        if memory_type_match(&payload, "attention") {
            if let Some(f) = payload.get("focus").and_then(|v| v.as_str()) {
                if focus.iter().any(|want| want == f) {
                    db.db_mut()
                        .delete(id)
                        .map_err(|e| format!("memory.attention.retire: {e}"))?;
                    removed += 1;
                    if !retired.iter().any(|r: &String| r == f) {
                        retired.push(f.to_string());
                    }
                }
            }
        }
    }
    db.flush()
        .map_err(|e| format!("memory.attention.retire flush: {e}"))?;
    ok_value(serde_json::json!({"removed": removed, "retired_focus": retired}))
}

fn source_refs_of(entry: &Value) -> Vec<String> {
    entry
        .get("source_refs")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

pub fn memory_experience_write(db: &mut TriviumDb, args: &Value) -> Result<Value, String> {
    let entries = arg_array(args, "entries", "memory.experience.write")?;
    if entries.is_empty() || entries.len() > MAX_WRITE_ENTRIES {
        return err(format!(
            "memory.experience.write: entries must contain 1..={MAX_WRITE_ENTRIES} items"
        ));
    }
    let mut ids = Vec::new();
    for entry in entries {
        let title = entry
            .get("title")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "memory.experience.write: entry missing 'title'".to_string())?;
        let summary = entry
            .get("summary")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "memory.experience.write: entry missing 'summary'".to_string())?;
        if title.trim().is_empty() || summary.trim().is_empty() {
            return err("memory.experience.write: title/summary must be non-empty");
        }
        let mut payload = serde_json::Map::new();
        payload.insert("title".into(), Value::String(title.to_string()));
        payload.insert("summary".into(), Value::String(summary.to_string()));
        payload.insert("_memory_type".into(), Value::String("experience".into()));
        let refs = source_refs_of(entry);
        if !refs.is_empty() {
            payload.insert("source_refs".into(), serde_json::json!(refs));
        }
        let vector = vec![0.0_f32; ZERO_DIM];
        let id = db
            .db_mut()
            .insert(&vector, Value::Object(payload))
            .map_err(|e| format!("memory.experience.write insert: {e}"))?;
        ids.push(id);
    }
    db.flush()
        .map_err(|e| format!("memory.experience.write flush: {e}"))?;
    ok_value(serde_json::json!({"written": ids.len(), "ids": ids}))
}

pub fn memory_preference_write(db: &mut TriviumDb, args: &Value) -> Result<Value, String> {
    let entries = arg_array(args, "entries", "memory.preference.write")?;
    if entries.is_empty() || entries.len() > MAX_WRITE_ENTRIES {
        return err(format!(
            "memory.preference.write: entries must contain 1..={MAX_WRITE_ENTRIES} items"
        ));
    }
    let mut ids = Vec::new();
    for entry in entries {
        let key = entry
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "memory.preference.write: entry missing 'key'".to_string())?;
        let value = entry
            .get("value")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "memory.preference.write: entry missing 'value'".to_string())?;
        if key.trim().is_empty() || value.trim().is_empty() {
            return err("memory.preference.write: key/value must be non-empty");
        }
        let mut payload = serde_json::Map::new();
        payload.insert("key".into(), Value::String(key.to_string()));
        payload.insert("value".into(), Value::String(value.to_string()));
        payload.insert("_memory_type".into(), Value::String("preference".into()));
        let refs = source_refs_of(entry);
        if !refs.is_empty() {
            payload.insert("source_refs".into(), serde_json::json!(refs));
        }
        let vector = vec![0.0_f32; ZERO_DIM];
        let id = db
            .db_mut()
            .insert(&vector, Value::Object(payload))
            .map_err(|e| format!("memory.preference.write insert: {e}"))?;
        ids.push(id);
    }
    db.flush()
        .map_err(|e| format!("memory.preference.write flush: {e}"))?;
    ok_value(serde_json::json!({"written": ids.len(), "ids": ids}))
}

fn delete_cognitive_nodes_matching(
    db: &mut TriviumDb,
    matcher: &dyn Fn(&Value) -> bool,
) -> Result<usize, String> {
    let mut removed = 0usize;
    for id in db.db().get_all_ids() {
        let Some(payload) = db.db().get_payload(id) else {
            continue;
        };
        if payload.get("_memory_type").and_then(|v| v.as_str()) == Some("cognitive")
            && matcher(&payload)
        {
            db.db_mut()
                .delete(id)
                .map_err(|e| format!("memory.cognitive.update delete: {e}"))?;
            removed += 1;
        }
    }
    Ok(removed)
}

fn delete_cognitive_edges_matching(
    db: &mut TriviumDb,
    matcher: &dyn Fn(&Value) -> bool,
) -> Result<usize, String> {
    let mut removed = 0usize;
    for id in db.db().get_all_ids() {
        let Some(payload) = db.db().get_payload(id) else {
            continue;
        };
        if payload.get("_memory_type").and_then(|v| v.as_str()) == Some("cognitive_edge")
            && matcher(&payload)
        {
            db.db_mut()
                .delete(id)
                .map_err(|e| format!("memory.cognitive.update edge delete: {e}"))?;
            removed += 1;
        }
    }
    Ok(removed)
}

pub fn memory_cognitive_update(db: &mut TriviumDb, args: &Value) -> Result<Value, String> {
    // 原子更新：所有输入先通过校验，再一次性变更并 flush。
    let nodes = args
        .get("nodes")
        .map(|v| v.as_array().cloned().unwrap_or_default())
        .unwrap_or_default();
    let edges = args
        .get("edges")
        .map(|v| v.as_array().cloned().unwrap_or_default())
        .unwrap_or_default();
    if nodes.is_empty() && edges.is_empty() {
        return err("memory.cognitive.update: nodes/edges must contain at least one update");
    }
    if nodes.len() + edges.len() > MAX_WRITE_ENTRIES {
        return err(format!(
            "memory.cognitive.update: total updates must be <= {MAX_WRITE_ENTRIES}"
        ));
    }

    // Validate all first (no mutation before this point).
    let mut node_ops = Vec::new();
    for node in &nodes {
        let action = node
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("upsert");
        let node_id = node
            .get("node_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let insight = node
            .get("insight")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let context = node
            .get("context")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        match action {
            "upsert" => {
                if insight.trim().is_empty() && context.trim().is_empty() {
                    return err("memory.cognitive.update: upsert node needs insight or context");
                }
                if node_id.as_ref().is_some_and(|id| id.trim().is_empty()) {
                    return err("memory.cognitive.update: node_id must not be empty");
                }
                node_ops.push((
                    action.to_string(),
                    node_id,
                    insight,
                    context,
                    None::<String>,
                    None::<String>,
                    None::<String>,
                ));
            }
            "delete" => {
                if node_id.is_none() && insight.trim().is_empty() {
                    return err("memory.cognitive.update: delete node needs node_id or insight");
                }
                node_ops.push((
                    action.to_string(),
                    node_id,
                    insight,
                    context,
                    None,
                    None,
                    None,
                ));
            }
            other => {
                return err(format!(
                    "memory.cognitive.update: unknown node action '{other}'"
                ))
            }
        }
    }
    let mut edge_ops = Vec::new();
    for edge in &edges {
        let action = edge
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("upsert");
        let from = edge
            .get("from")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let to = edge
            .get("to")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let relation = edge
            .get("relation")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if from.trim().is_empty() || to.trim().is_empty() || relation.trim().is_empty() {
            return err("memory.cognitive.update: edge needs from/to/relation");
        }
        match action {
            "upsert" | "delete" => edge_ops.push((action.to_string(), from, to, relation)),
            other => {
                return err(format!(
                    "memory.cognitive.update: unknown edge action '{other}'"
                ))
            }
        }
    }

    let mut node_changes = 0usize;
    let mut edge_changes = 0usize;
    let vector = vec![0.0_f32; ZERO_DIM];

    for (action, node_id, insight, context, _, _, _) in node_ops {
        if let Some(ref nid) = node_id {
            let nid = nid.clone();
            delete_cognitive_nodes_matching(db, &move |p| {
                p.get("node_id").and_then(|v| v.as_str()) == Some(nid.as_str())
            })?;
        }
        if action == "delete" {
            if node_id.is_none() {
                let insight = insight.clone();
                node_changes += delete_cognitive_nodes_matching(db, &move |p| {
                    p.get("insight").and_then(|v| v.as_str()) == Some(insight.as_str())
                })?;
            } else {
                node_changes += 1;
            }
        } else {
            let mut payload = serde_json::Map::new();
            payload.insert("_memory_type".into(), Value::String("cognitive".into()));
            if let Some(nid) = node_id {
                payload.insert("node_id".into(), Value::String(nid));
            }
            if !insight.trim().is_empty() {
                payload.insert("insight".into(), Value::String(insight));
            }
            if !context.trim().is_empty() {
                payload.insert("context".into(), Value::String(context));
            }
            db.db_mut()
                .insert(&vector, Value::Object(payload))
                .map_err(|e| format!("memory.cognitive.update node insert: {e}"))?;
            node_changes += 1;
        }
    }

    for (action, from, to, relation) in edge_ops {
        if action == "upsert" {
            let payload = serde_json::json!({
                "_memory_type": "cognitive_edge",
                "from": from,
                "to": to,
                "from_entity": from,
                "to_entity": to,
                "relation": relation,
            });
            db.db_mut()
                .insert(&vector, payload)
                .map_err(|e| format!("memory.cognitive.update edge insert: {e}"))?;
            edge_changes += 1;
        } else {
            let from_c = from.clone();
            let to_c = to.clone();
            edge_changes += delete_cognitive_edges_matching(db, &move |p| {
                (p.get("from").and_then(|v| v.as_str()) == Some(from_c.as_str())
                    || p.get("from_entity").and_then(|v| v.as_str()) == Some(from_c.as_str()))
                    && (p.get("to").and_then(|v| v.as_str()) == Some(to_c.as_str())
                        || p.get("to_entity").and_then(|v| v.as_str()) == Some(to_c.as_str()))
            })?;
        }
    }

    db.flush()
        .map_err(|e| format!("memory.cognitive.update flush: {e}"))?;
    ok_value(serde_json::json!({
        "success": true,
        "node_changes": node_changes,
        "edge_changes": edge_changes,
    }))
}

pub fn memory_evidence_lookup(
    thought_store: Option<&Arc<ThoughtStore>>,
    args: &Value,
) -> Result<Value, String> {
    let refs = arg_array(args, "source_refs", "memory.evidence.lookup")?;
    let refs: Vec<String> = refs
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();
    if refs.is_empty() {
        return err("memory.evidence.lookup: source_refs must be a non-empty string array");
    }
    let limit = arg_limit(args);
    let Some(store) = thought_store else {
        return err("memory.evidence.lookup: thought store is not configured");
    };
    let timeline = store
        .recover()
        .map_err(|e| format!("memory.evidence.lookup: {e}"))?;
    let mut items = Vec::new();
    for group in timeline.groups.iter().rev() {
        for ctx in group.contexts.iter() {
            if items.len() >= limit {
                break;
            }
            if !refs.iter().any(|r| r == &ctx.thought_id.to_string()) {
                continue;
            }
            let input = format_thinking_input_for_evidence(&ctx.input);
            let output = ctx.output.as_ref().map(|o| {
                let mut parts = Vec::new();
                if let Some(think) = o.think.as_ref().filter(|s| !s.is_empty()) {
                    parts.push(format!("think: {think}"));
                }
                if let Some(say) = o.say.as_ref().filter(|s| !s.is_empty()) {
                    parts.push(format!("say: {say}"));
                }
                parts.join("\n")
            });
            items.push(serde_json::json!({
                "thought_id": ctx.thought_id.to_string(),
                "input": input,
                "output": output,
            }));
        }
    }
    ok_value(serde_json::json!({"items": items, "count": items.len()}))
}

fn format_thinking_input_for_evidence(input: &crate::agent::thought::ThinkingInput) -> String {
    use crate::agent::thought::ThinkingInput;
    match input {
        ThinkingInput::User { text } => text.clone(),
        ThinkingInput::PlatformEcho { summary, .. }
        | ThinkingInput::ReflectOnly { summary }
        | ThinkingInput::CapabilityResult { summary, .. } => summary.clone(),
        ThinkingInput::ModeTrigger { mode, reason } => format!("[{mode}] {reason}"),
    }
}
