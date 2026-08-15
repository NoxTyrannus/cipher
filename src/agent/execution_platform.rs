use crate::agent::thought::ThoughtId;
use crate::common::{AgentError, Result, UtcTimestamp};
use crate::data::duckdb::Registry;
use crate::data::platform_cursor::CursorStore;
use crate::data::platform_product_store::{PlatformProductStore, ProductType};
use crate::data::triviumdb::insert_raw_file_node;
use crate::data::triviumdb::TriviumDb;
use crate::data::ModelRow;
use crate::logic::capability::executor::CapabilityExecutor;
use crate::logic::capability::service::{CapabilityCall, CapabilityService};
use crate::logic::model::message::{ChatMessage, SystemKind};
use crate::logic::model::prompts::read_platform_prompt;
use crate::logic::model::provider::{LlmProvider, LlmRequest};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;
use uuid::Uuid;

use super::agent_pool::AgentPool;
use super::communication::{
    AgentMessage, ExecutionDag, ExecutionOutput, ExecutionStatus, NodeResult, NodeStatus,
    ThinkDecision,
};
use super::subagent::{SubAgentInstance, SubAgentPool, SubAgentStatus};

fn select_prompt(kind: &str, prompts_dir: &Path) -> String {
    if kind.is_empty() || kind == "normal" {
        return read_platform_prompt(prompts_dir, "execution_platform.md");
    }
    let specialized = format!("execution_platform_{kind}.md");
    if prompts_dir.join(&specialized).exists() {
        read_platform_prompt(prompts_dir, &specialized)
    } else {
        read_platform_prompt(prompts_dir, "execution_platform.md")
    }
}

/// prefilled_arguments 总大小超过该阈值时禁止预填, 自动降级为 subagent 执行。
const PREFILLED_MAX_BYTES: usize = 8192;

/// subagent 循环最大轮数 (模型不可配置)。
const SUBAGENT_MAX_TURNS: u32 = 6;

/// 从注册表默认 agent 的 config.max_turns 读取 subagent 轮数默认值。
/// 未配置或非数字时返回 None (调用方回退 SUBAGENT_MAX_TURNS)。
fn agent_max_turns_from_registry(registry: &crate::data::duckdb::loader::Registry) -> Option<u32> {
    let agent = registry
        .agents
        .values()
        .find(|a| a.is_default)
        .or_else(|| registry.agents.get("agent"))?;
    let config = agent.config.as_ref()?;
    let max_turns = config.get("max_turns")?;
    max_turns.as_u64().and_then(|v| u32::try_from(v).ok())
}

/// 设计解析失败后的重试提示: 原错误信息原样保留, 并附加具体修复指引。
fn design_retry_prompt(base_prompt: &str, first_error: &str) -> String {
    format!(
        "{base_prompt}\n\n## 上次输出解析失败\n{first_error}\n\
         请修正输出使其是**单个完整 JSON 对象** (不要多余闭合括号/不要截断), 重新输出。\n\
         若失败原因是嵌入了过长的内容（如文件全文/大段文本），不要把它放进 \
         prefilled_arguments——省略 prefilled_arguments，subagent 会在执行时生成内容；\
         或对 file.write 类节点改为先用 shell.exec 分块写入。\n\
         prefilled_arguments 必须严格匹配能力 schema 的 required 字段与类型。"
    )
}

/// 判定 prefilled_arguments 是否超限。返回实际字节数; 未超限返回 None。
fn prefilled_arguments_oversized(args: &serde_json::Value) -> Option<usize> {
    let size = serde_json::to_string(args).map(|s| s.len()).unwrap_or(0);
    (size > PREFILLED_MAX_BYTES).then_some(size)
}

#[cfg(test)]
const UNRECOGNIZED_KIND: &str = "__unrecognized_template__";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentDesign {
    pub template_kind: String,
    pub capability_ids: Vec<String>,
    pub task_context: String,

    #[serde(default)]
    pub arguments: Option<serde_json::Value>,
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DagDesign {
    pub template_kind: String,
    pub nodes: Vec<DagNodeDesign>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DagNodeDesign {
    pub id: String,
    pub capability_ids: Vec<String>,
    pub task_context: String,

    #[serde(default)]
    pub arguments: Option<serde_json::Value>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u32,
}

#[derive(Debug, Clone)]
pub enum ExecutionDesign {
    Single(SubAgentDesign),
    Dag(DagDesign),

    Flow(TaskFlow),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskFlow {
    pub template_kind: String,

    #[serde(default)]
    pub trigger: Option<TriggerSpec>,
    pub nodes: Vec<TaskNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum TriggerSpec {
    Cron { schedule: String },

    Webhook { url: String },

    Event { kind: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskNode {
    pub id: String,

    #[serde(default)]
    pub depends_on: Vec<String>,

    pub task_description: String,

    #[serde(default)]
    pub expected_output: String,

    pub capability: String,

    #[serde(default)]
    pub prefilled_arguments: Option<serde_json::Value>,
}

fn resolve_capability_alias(name: &str) -> &str {
    let name = name.strip_prefix("functions.").unwrap_or(name);
    match name {
        "shell_exec" | "shell" | "bash" => "shell.exec",
        "file_read" | "read" => "file.read",
        "file_write" | "write" => "file.write",
        "file_list" | "ls" => "file.list",
        "file_move" | "mv" => "file.move",
        "file_delete" | "rm" => "file.delete",
        "code_exec" | "code" => "code.exec",
        "text_grep" | "grep" => "text.grep",
        "db_query" | "query" => "db.query",
        other => other,
    }
}

fn default_max_turns() -> u32 {
    10
}
fn default_timeout_seconds() -> u32 {
    600
}

fn parse_execution_design(content: &str) -> Result<ExecutionDesign> {
    probe_parse_execution_design(content).0
}

/// 供探针 (examples/exec_probe) 使用的解析入口: 返回 (解析结果, 尝试次数)。
/// 直接解析计 1 次; 直接失败且 repair 后重试计 2 次。
#[doc(hidden)]
pub fn probe_parse_execution_design(content: &str) -> (Result<ExecutionDesign>, u32) {
    let cleaned = preprocess_llm_json(content);

    match parse_design_attempt(&cleaned) {
        Ok(design) => (Ok(design), 1),
        Err(first_error) => {
            let repaired = crate::common::json_util::repair_json(&cleaned);
            if repaired != cleaned {
                tracing::warn!(
                    "execution_platform: 设计 JSON 直接解析失败, repair 后重试: {first_error}"
                );
                let retried = parse_design_attempt(&repaired).map_err(|second_error| {
                    AgentError::Parse(format!(
                        "execution_platform: repair 后仍解析失败: {second_error} (原错误: {first_error})"
                    ))
                });
                (retried, 2)
            } else {
                (Err(first_error), 1)
            }
        }
    }
}

fn parse_design_attempt(cleaned: &str) -> Result<ExecutionDesign> {
    if let Ok(dag) = serde_json::from_str::<DagDesign>(cleaned) {
        if dag.template_kind == "dag" && !dag.nodes.is_empty() {
            return Ok(ExecutionDesign::Dag(dag));
        }
    }
    if has_nodes_signature(cleaned) {
        match parse_task_flow_tolerant(cleaned) {
            Ok(design) => return Ok(design),
            Err(parse_error) => match extract_flow_nodes_tolerant(cleaned) {
                Some(flow) => {
                    tracing::warn!(
                            "execution_platform: TaskFlow 整体解析失败, 逐节点提取兜底恢复 {} 节点: {parse_error}",
                            flow.nodes.len()
                        );
                    return Ok(ExecutionDesign::Flow(flow));
                }
                None => return Err(parse_error),
            },
        }
    }

    if let Some(flow) = extract_flow_nodes_tolerant(cleaned) {
        tracing::warn!(
            "execution_platform: TaskFlow 无有效整体签名, 逐节点提取兜底恢复 {} 节点",
            flow.nodes.len()
        );
        return Ok(ExecutionDesign::Flow(flow));
    }

    if let Ok(single) = serde_json::from_str::<SubAgentDesign>(cleaned) {
        return Ok(ExecutionDesign::Single(single));
    }

    Err(AgentError::Parse(format!(
        "execution_platform: failed to parse ExecutionDesign from LLM content: {}",
        crate::common::json_util::truncate_utf8_boundary(cleaned, 2000)
    )))
}

fn extract_flow_nodes_tolerant(content: &str) -> Option<TaskFlow> {
    let cleaned = preprocess_llm_json(content);

    let mut template_kind = "normal".to_string();
    let mut trigger = None;
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&cleaned) {
        template_kind = value
            .get("template_kind")
            .and_then(|v| v.as_str())
            .unwrap_or("normal")
            .to_string();
        trigger = value
            .get("trigger")
            .cloned()
            .and_then(|v| serde_json::from_value::<TriggerSpec>(v).ok());
    }

    let mut nodes = Vec::new();
    let chars: Vec<char> = cleaned.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '{' {
            i += 1;
            continue;
        }

        let mut depth = 0i32;
        let mut in_string = false;
        let mut escaped = false;
        let mut j = i;
        let mut end = None;
        while j < chars.len() {
            let c = chars[j];
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = !in_string;
            } else if !in_string {
                if c == '{' {
                    depth += 1;
                } else if c == '}' {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(j);
                        break;
                    }
                }
            }
            j += 1;
        }
        let Some(end_pos) = end else { break };
        let fragment: String = chars[i..=end_pos].iter().collect();

        if fragment.contains("\"id\"") {
            if let Ok(node) = serde_json::from_str::<TaskNode>(&fragment) {
                nodes.push(node);

                i = end_pos + 1;
                continue;
            }
        }
        i += 1;
    }

    if nodes.is_empty() {
        return None;
    }
    Some(TaskFlow {
        template_kind,
        trigger,
        nodes,
    })
}

fn preprocess_llm_json(content: &str) -> String {
    let mut text = content.trim().to_string();

    if let Some(start) = text.find("```json") {
        if let Some(end) = text[start + 7..].find("```") {
            text = text[start + 7..start + 7 + end].trim().to_string();
        }
    } else if text.starts_with("```") {
        text = text.trim_matches('`').trim().to_string();
    }

    let mut result = text.trim().to_string();
    while result.ends_with(',') {
        result.pop();
        result = result.trim_end().to_string();
    }
    let result = result.replace(",}", "}").replace(",]", "]");

    let mut balanced = String::with_capacity(result.len() + 8);
    let mut open_braces = 0i32;
    let mut open_brackets = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for ch in result.chars() {
        if escaped {
            balanced.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' => {
                balanced.push(ch);
                escaped = true;
            }
            '"' => {
                in_string = !in_string;
                balanced.push(ch);
            }
            '{' if !in_string => {
                open_braces += 1;
                balanced.push('{');
            }
            '}' if !in_string => {
                if open_braces > 0 {
                    open_braces -= 1;
                    balanced.push('}');
                }
            }
            '[' if !in_string => {
                open_brackets += 1;
                balanced.push('[');
            }
            ']' if !in_string => {
                if open_brackets > 0 {
                    open_brackets -= 1;
                    balanced.push(']');
                }
            }
            _ => balanced.push(ch),
        }
    }
    for _ in 0..open_brackets.max(0) {
        balanced.push(']');
    }
    for _ in 0..open_braces.max(0) {
        balanced.push('}');
    }
    balanced
}

fn has_nodes_signature(content: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(content) else {
        return false;
    };
    value.get("nodes").and_then(|v| v.as_array()).is_some()
}

fn parse_task_flow_tolerant(content: &str) -> Result<ExecutionDesign> {
    let value: serde_json::Value = serde_json::from_str(content).map_err(|e| {
        AgentError::Parse(format!("execution_platform: TaskFlow JSON invalid: {e}"))
    })?;
    let template_kind = value
        .get("template_kind")
        .and_then(|v| v.as_str())
        .unwrap_or("normal")
        .to_string();
    let trigger = value
        .get("trigger")
        .cloned()
        .and_then(|v| serde_json::from_value::<TriggerSpec>(v).ok());
    let nodes_value = value
        .get("nodes")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut nodes = Vec::new();
    let mut failures = Vec::new();
    for (index, node_value) in nodes_value.iter().enumerate() {
        match serde_json::from_value::<TaskNode>(node_value.clone()) {
            Ok(node) => nodes.push(node),
            Err(e) => {
                let id = node_value
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string();
                failures.push(format!("node[{index}] id={id}: {e}"));
            }
        }
    }

    if nodes.is_empty() {
        return Err(AgentError::Parse(format!(
            "execution_platform: TaskFlow 无可执行节点 (全部解析失败): {}; {}",
            failures.join("; "),
            crate::common::json_util::truncate_utf8_boundary(content, 2000)
        )));
    }
    if !failures.is_empty() {
        tracing::warn!(
            "execution_platform: TaskFlow 部分节点解析失败 ({} 跳过): {}",
            failures.len(),
            failures.join("; ")
        );
    }
    Ok(ExecutionDesign::Flow(TaskFlow {
        template_kind,
        trigger,
        nodes,
    }))
}

fn extract_json_block(text: &str) -> Option<String> {
    let start = text.find("```json")?;
    let after_start = &text[start + 7..];
    let end = after_start.find("```")?;
    Some(after_start[..end].trim().to_string())
}

fn parse_schedule_to_seconds(schedule: &str) -> Option<u64> {
    let s = schedule.trim().to_lowercase();
    if let Ok(n) = s.parse::<u64>() {
        return Some(n.max(1));
    }
    if let Some(n) = s
        .strip_suffix('s')
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        return Some(n.max(1));
    }
    if let Some(n) = s
        .strip_suffix('m')
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        return Some((n * 60).max(1));
    }

    if let Some(n) = s.strip_prefix("*/").and_then(|v| v.parse::<u64>().ok()) {
        return Some((n * 60).max(1));
    }
    None
}

fn webhook_listen_addr(url: &str) -> Option<String> {
    let trimmed = url.trim();
    let after_scheme = trimmed
        .strip_prefix("http://")
        .or_else(|| trimmed.strip_prefix("https://"))
        .unwrap_or(trimmed);
    let host_port = after_scheme.split('/').next().unwrap_or("");
    if host_port.is_empty() {
        return None;
    }
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => (h, p.to_string()),
        _ => (host_port, "8080".to_string()),
    };
    let host = if host.is_empty() { "127.0.0.1" } else { host };
    Some(format!("{host}:{port}"))
}

#[derive(Debug, Clone)]
enum SubagentAction {
    Arguments { arguments: serde_json::Value },

    Done { summary: String },

    Invalid(String),
}

fn parse_subagent_output(content: &str) -> SubagentAction {
    let mut specific_reason: Option<String> = None;
    for candidate in subagent_parse_candidates(content) {
        match parse_subagent_action_json(&candidate) {
            Ok(action) => return action,
            Err(SubagentParseError::ArgumentsNotObject(reason)) => {
                if specific_reason.is_none() {
                    specific_reason = Some(reason);
                }
            }
            Err(SubagentParseError::NotAction) => {}
        }
        let repaired = crate::common::json_util::repair_json(&candidate);
        if repaired != candidate {
            tracing::warn!("parse_subagent_output: 直接解析失败, repair 后重试");
            if let Ok(action) = parse_subagent_action_json(&repaired) {
                return action;
            }
        }
    }
    SubagentAction::Invalid(specific_reason.unwrap_or_else(|| {
        format!(
            "输出缺少 arguments 或 done 字段或 JSON 非法: {}",
            crate::common::json_util::truncate_utf8_boundary(content, 120)
        )
    }))
}

/// 逐级 fallback 候选链: 围栏块 → 剥离推理前言后取首个 JSON 对象 → 整串。
fn subagent_parse_candidates(content: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    if let Some(block) = extract_json_block(content) {
        candidates.push(block);
    }
    let stripped = crate::common::json_util::strip_reasoning_preamble(content);
    if let Some(obj) = crate::common::json_util::extract_first_json_object(&stripped) {
        candidates.push(obj);
    }
    let trimmed = content.trim().to_string();
    if !candidates.contains(&trimmed) {
        candidates.push(trimmed);
    }
    candidates
}

/// subagent 动作解析错误分类。
enum SubagentParseError {
    /// JSON 非法或未识别为动作, 无明确原因, 走通用 Invalid 文案。
    NotAction,
    /// arguments 存在但非 JSON 对象, reason 直接反馈给模型。
    ArgumentsNotObject(String),
}

fn parse_subagent_action_json(
    json_text: &str,
) -> std::result::Result<SubagentAction, SubagentParseError> {
    let value: serde_json::Value =
        serde_json::from_str(json_text).map_err(|_| SubagentParseError::NotAction)?;
    if value.get("arguments").is_some() {
        let args = value.get("arguments").unwrap();
        if args.is_object() {
            return Ok(SubagentAction::Arguments {
                arguments: args.clone(),
            });
        }
        return Err(SubagentParseError::ArgumentsNotObject(
            "arguments 必须是 JSON 对象".to_string(),
        ));
    }
    if value.get("tool_call").is_some() {
        let tc = value.get("tool_call").unwrap();
        if let Some(args) = tc.get("arguments") {
            if args.is_object() {
                return Ok(SubagentAction::Arguments {
                    arguments: args.clone(),
                });
            }
        }
        return Err(SubagentParseError::ArgumentsNotObject(
            "tool_call.arguments 必须是 JSON 对象".to_string(),
        ));
    }
    if value.get("done").and_then(|v| v.as_bool()) == Some(true) {
        let summary = value
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        return Ok(SubagentAction::Done { summary });
    }
    Err(SubagentParseError::NotAction)
}

trait TopoNode {
    fn node_id(&self) -> &str;
    fn node_deps(&self) -> &[String];
}

impl TopoNode for DagNodeDesign {
    fn node_id(&self) -> &str {
        &self.id
    }
    fn node_deps(&self) -> &[String] {
        &self.depends_on
    }
}

impl TopoNode for TaskNode {
    fn node_id(&self) -> &str {
        &self.id
    }
    fn node_deps(&self) -> &[String] {
        &self.depends_on
    }
}

fn topological_sort<T: TopoNode>(nodes: &[T]) -> Result<Vec<String>> {
    let mut graph: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut in_degree: HashMap<&str, usize> = HashMap::new();

    for node in nodes {
        graph.entry(node.node_id()).or_default();
        in_degree.entry(node.node_id()).or_insert(0);
    }

    for node in nodes {
        for dep in node.node_deps() {
            if !graph.contains_key(dep.as_str()) {
                return Err(AgentError::Parse(format!(
                    "topological_sort: node '{}' depends on unknown node '{}'",
                    node.node_id(),
                    dep
                )));
            }
            graph.get_mut(dep.as_str()).unwrap().push(node.node_id());
            *in_degree.get_mut(node.node_id()).unwrap() += 1;
        }
    }

    let mut queue: VecDeque<&str> = VecDeque::new();
    for (id, degree) in in_degree.iter() {
        if *degree == 0 {
            queue.push_back(id);
        }
    }

    let mut sorted: Vec<String> = Vec::with_capacity(nodes.len());
    while let Some(node_id) = queue.pop_front() {
        sorted.push(node_id.to_string());
        if let Some(dependents) = graph.get(node_id) {
            for dep_id in dependents {
                let deg = in_degree.get_mut(dep_id).unwrap();
                *deg -= 1;
                if *deg == 0 {
                    queue.push_back(dep_id);
                }
            }
        }
    }

    if sorted.len() != nodes.len() {
        return Err(AgentError::Parse(
            "topological_sort: DAG contains a cycle".to_string(),
        ));
    }

    Ok(sorted)
}

fn topological_layers<T: TopoNode>(nodes: &[T]) -> Result<Vec<Vec<String>>> {
    let mut graph: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    for node in nodes {
        graph.entry(node.node_id()).or_default();
        in_degree.entry(node.node_id()).or_insert(0);
    }
    for node in nodes {
        for dep in node.node_deps() {
            if !graph.contains_key(dep.as_str()) {
                return Err(AgentError::Parse(format!(
                    "topological_layers: node '{}' depends on unknown node '{}'",
                    node.node_id(),
                    dep
                )));
            }
            graph.get_mut(dep.as_str()).unwrap().push(node.node_id());
            *in_degree.get_mut(node.node_id()).unwrap() += 1;
        }
    }

    let mut layers: Vec<Vec<String>> = Vec::new();
    let mut processed = 0usize;
    loop {
        let layer: Vec<String> = in_degree
            .iter()
            .filter(|(_, &deg)| deg == 0)
            .map(|(id, _)| (*id).to_string())
            .collect();
        if layer.is_empty() {
            break;
        }
        for id in &layer {
            in_degree.remove(id.as_str());
            processed += 1;
            if let Some(dependents) = graph.get(id.as_str()) {
                for dep_id in dependents {
                    *in_degree.get_mut(dep_id).unwrap() -= 1;
                }
            }
        }
        layers.push(layer);
    }

    if processed != nodes.len() {
        return Err(AgentError::Parse(
            "topological_layers: DAG contains a cycle".to_string(),
        ));
    }
    Ok(layers)
}

fn build_dep_summary(
    node: &TaskNode,
    results: &HashMap<String, NodeResult>,
    by_id: &HashMap<String, &TaskNode>,
) -> String {
    node.depends_on
        .iter()
        .filter_map(|d| results.get(d))
        .map(|r| {
            let cap = by_id
                .get(&r.node_id)
                .map(|n| n.capability.as_str())
                .unwrap_or("?");
            let summary: String = r.summary.chars().take(200).collect();
            if summary.is_empty() {
                format!("[{}] {} 完成 (无输出摘要)", r.node_id, cap)
            } else {
                format!("[{}] {} 完成: {}", r.node_id, cap, summary)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn sanitize_args_summary(args: &serde_json::Value) -> String {
    let Some(obj) = args.as_object() else {
        let clipped: String = args.to_string().chars().take(80).collect();
        return format!("args_keys=[non-object], args_prefix=\"{clipped}\"");
    };
    let keys: Vec<&str> = obj.keys().map(|k| k.as_str()).collect();
    let mut detail = Vec::new();
    for (k, v) in obj {
        let lower = k.to_lowercase();
        if lower.contains("api_key")
            || lower.contains("token")
            || lower.contains("secret")
            || lower.contains("password")
        {
            detail.push(format!("{k}=<redacted>"));
        } else if let Some(s) = v.as_str() {
            if k == "path" || k.ends_with("_path") {
                detail.push(format!("{k}=\"{s}\""));
            } else {
                let len = s.chars().count();
                let prefix: String = s.chars().take(80).collect();
                detail.push(format!("{k}_len={len}, {k}_prefix=\"{prefix}\""));
            }
        } else {
            let value: String = v.to_string().chars().take(80).collect();
            detail.push(format!("{k}={value}"));
        }
    }
    if detail.is_empty() {
        format!("args_keys=[{}]", keys.join(", "))
    } else {
        format!("args_keys=[{}], {}", keys.join(", "), detail.join(", "))
    }
}

#[derive(Clone)]
struct NodeRunner {
    registry: Option<Registry>,
    executor: Option<Arc<CapabilityExecutor>>,
    provider: Arc<dyn LlmProvider>,
    model_row: ModelRow,
    api_key: SecretString,
}

impl NodeRunner {
    pub async fn execute_flow_public(&self, flow: &TaskFlow) -> Vec<NodeResult> {
        let layers = match topological_layers(&flow.nodes) {
            Ok(l) => l,
            Err(e) => {
                tracing::error!("scheduler: flow cycle detected: {e}");
                return flow
                    .nodes
                    .iter()
                    .map(|n| NodeResult {
                        node_id: n.id.clone(),
                        status: NodeStatus::Failed,
                        summary: String::new(),
                        error: Some(format!("flow cycle: {e}")),
                        tool_call_count: 0,
                        tool_call_logs: vec![],
                    })
                    .collect();
            }
        };
        let by_id: HashMap<String, &TaskNode> =
            flow.nodes.iter().map(|n| (n.id.clone(), n)).collect();
        let mut results: HashMap<String, NodeResult> = HashMap::new();
        let mut node_results = Vec::new();
        for layer in &layers {
            let mut join_set = tokio::task::JoinSet::new();
            for node_id in layer {
                let node = by_id[node_id.as_str()];
                let failed_dep = node.depends_on.iter().find(|dep| {
                    results
                        .get(*dep)
                        .is_some_and(|r| r.status != NodeStatus::Completed)
                });
                if let Some(dep) = failed_dep {
                    node_results.push(NodeResult {
                        node_id: node.id.clone(),
                        status: NodeStatus::Skipped,
                        summary: String::new(),
                        error: Some(format!("dependency '{dep}' failed/skipped")),
                        tool_call_count: 0,
                        tool_call_logs: vec![],
                    });
                    continue;
                }
                let dep_summary = build_dep_summary(node, &results, &by_id);
                let node_owned = node.clone();
                let runner = self.clone();
                if let Some(args) = node.prefilled_arguments.clone() {
                    match prefilled_arguments_oversized(&args) {
                        Some(bytes) => {
                            tracing::warn!(
                                "execution_platform: prefilled degraded: oversized ({bytes} bytes > {PREFILLED_MAX_BYTES}), node '{}' 改走 subagent",
                                node.id
                            );
                            join_set.spawn(async move {
                                runner.run_subagent_loop(&node_owned, &dep_summary).await.0
                            });
                        }
                        None => {
                            join_set.spawn(async move {
                                runner.execute_prefilled_node(&node_owned, &args).await
                            });
                        }
                    }
                } else {
                    join_set.spawn(async move {
                        runner.run_subagent_loop(&node_owned, &dep_summary).await.0
                    });
                }
            }
            while let Some(outcome) = join_set.join_next().await {
                if let Ok(result) = outcome {
                    results.insert(result.node_id.clone(), result.clone());
                    node_results.push(result);
                }
            }
        }
        node_results
    }

    async fn execute_prefilled_node(
        &self,
        node: &TaskNode,
        arguments: &serde_json::Value,
    ) -> NodeResult {
        let canonical = resolve_capability_alias(&node.capability).to_string();

        let args_summary = sanitize_args_summary(arguments);
        tracing::info!(
            "execution_platform: flow node '{}' prefilled {} (one-shot)",
            node.id,
            canonical
        );
        let mut logs = vec![
            format!("node_id: {}", node.id),
            format!("prefilled_call: {canonical}"),
            format!("arguments: {args_summary}"),
        ];
        let capability_name = self
            .registry
            .as_ref()
            .and_then(|r| {
                r.base_capabilities
                    .get(&canonical)
                    .map(|row| row.name.clone())
                    .or_else(|| {
                        r.composite_capabilities
                            .get(&canonical)
                            .map(|row| row.name.clone())
                    })
            })
            .unwrap_or_else(|| canonical.clone());
        let cap_call = CapabilityCall {
            capability_id: canonical.clone(),
            capability_name,
            arguments: arguments.clone(),
        };
        let service = match self.capability_service() {
            Ok(Some(s)) => s,
            Ok(None) => {
                logs.push("NO_RUNTIME: registry/executor 未配置".to_string());
                return NodeResult {
                    node_id: node.id.clone(),
                    status: NodeStatus::Failed,
                    summary: String::new(),
                    error: Some("no capability runtime".to_string()),
                    tool_call_count: 0,
                    tool_call_logs: logs,
                };
            }
            Err(e) => {
                logs.push(format!("capability service init failed: {e}"));
                return NodeResult {
                    node_id: node.id.clone(),
                    status: NodeStatus::Failed,
                    summary: String::new(),
                    error: Some(e),
                    tool_call_count: 0,
                    tool_call_logs: logs,
                };
            }
        };
        match service.execute_for_agent("agent", &cap_call) {
            Ok(result) => {
                if let Some(fail) = Self::output_indicates_failure(&result.output) {
                    tracing::warn!(
                        "execution_platform: flow node '{}' prefilled FAIL {canonical}: {fail}; args={args_summary}",
                        node.id
                    );
                    logs.push(format!("FAIL {canonical}: {fail}"));
                    NodeResult {
                        node_id: node.id.clone(),
                        status: NodeStatus::Failed,
                        summary: String::new(),
                        error: Some(format!("{canonical}: {fail}")),
                        tool_call_count: 1,
                        tool_call_logs: logs,
                    }
                } else {
                    let summary: String = result.output.to_string().chars().take(300).collect();
                    tracing::info!(
                        "execution_platform: flow node '{}' prefilled OK {canonical}",
                        node.id
                    );
                    logs.push(format!("OK {canonical}: {summary}"));
                    NodeResult {
                        node_id: node.id.clone(),
                        status: NodeStatus::Completed,
                        summary,
                        error: None,
                        tool_call_count: 1,
                        tool_call_logs: logs,
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    "execution_platform: flow node '{}' prefilled FAIL {canonical}: {e}; args={args_summary}",
                    node.id
                );
                logs.push(format!("FAIL {canonical}: {e}"));
                NodeResult {
                    node_id: node.id.clone(),
                    status: NodeStatus::Failed,
                    summary: String::new(),
                    error: Some(format!("{canonical}: {e}")),
                    tool_call_count: 1,
                    tool_call_logs: logs,
                }
            }
        }
    }

    /// 返回 (NodeResult, 实际轮数)。
    async fn run_subagent_loop(&self, node: &TaskNode, dep_summary: &str) -> (NodeResult, u32) {
        tracing::info!(
            "execution_platform: flow node '{}' subagent loop (two-stage), task='{}'",
            node.id,
            node.task_description
        );
        let mut logs = vec![format!("node_id: {}", node.id)];
        let capability = resolve_capability_alias(&node.capability).to_string();
        let schema = self
            .registry
            .as_ref()
            .and_then(|r| {
                r.base_capabilities
                    .get(&capability)
                    .map(|row| row.schema_in.clone())
                    .or_else(|| {
                        r.composite_capabilities
                            .get(&capability)
                            .map(|_row| serde_json::json!({}))
                    })
            })
            .unwrap_or_else(|| serde_json::json!({}));
        let system_prompt = format!(
            "你是任务执行子代理。你的任务:\n{}\n\n期望产物:\n{}\n\n{}你必须使用能力 **{}** 完成任务。\n能力 schema:\n{}\n\n输出 JSON:\n- 生成参数调用: {{\"arguments\": {{...}}}}\n- 任务完成: {{\"done\": true, \"summary\": \"<结果摘要>\"}}\n\n规则:\n- arguments 必须严格符合 schema, 不要写散文到参数\n- 调用失败时分析错误并调整参数重试\n- 完成后输出 done",
            node.task_description,
            node.expected_output,
            if dep_summary.is_empty() { String::new() } else { format!("依赖节点结果:\n{dep_summary}\n\n") },
            capability,
            schema,
        );
        let mut messages = vec![
            ChatMessage::System {
                text: system_prompt,
                kind: SystemKind::Primary,
            },
            ChatMessage::User {
                text: "开始执行任务。只输出 JSON。".to_string(),
            },
        ];
        let max_turns = self
            .registry
            .as_ref()
            .and_then(agent_max_turns_from_registry)
            .unwrap_or(SUBAGENT_MAX_TURNS);
        if max_turns != SUBAGENT_MAX_TURNS {
            logs.push(format!("max_turns: {} (agent.config 覆盖)", max_turns));
        }
        let mut tool_call_count = 0u32;
        for turn in 0..max_turns {
            let req = match LlmRequest::from_model_row(
                &self.model_row,
                messages.clone(),
                self.api_key.clone(),
            ) {
                Ok(r) => r,
                Err(e) => {
                    logs.push(format!("LLM request build failed: {e}"));
                    return (
                        NodeResult {
                            node_id: node.id.clone(),
                            status: NodeStatus::Failed,
                            summary: String::new(),
                            error: Some(format!("LLM request build failed: {e}")),
                            tool_call_count,
                            tool_call_logs: logs,
                        },
                        turn + 1,
                    );
                }
            };
            let resp = match self.provider.call(&req).await {
                Ok(r) => r,
                Err(e) => {
                    logs.push(format!("LLM call failed (turn {turn}): {e}"));
                    return (
                        NodeResult {
                            node_id: node.id.clone(),
                            status: NodeStatus::Failed,
                            summary: String::new(),
                            error: Some(format!("subagent LLM call failed: {e}")),
                            tool_call_count,
                            tool_call_logs: logs,
                        },
                        turn + 1,
                    );
                }
            };
            match parse_subagent_output(&resp.content) {
                SubagentAction::Done { summary } => {
                    logs.push(format!("DONE: {summary}"));
                    return (
                        NodeResult {
                            node_id: node.id.clone(),
                            status: NodeStatus::Completed,
                            summary,
                            error: None,
                            tool_call_count,
                            tool_call_logs: logs,
                        },
                        turn + 1,
                    );
                }
                SubagentAction::Arguments { arguments } => {
                    let capability_name = self
                        .registry
                        .as_ref()
                        .and_then(|r| {
                            r.base_capabilities
                                .get(&capability)
                                .map(|row| row.name.clone())
                                .or_else(|| {
                                    r.composite_capabilities
                                        .get(&capability)
                                        .map(|row| row.name.clone())
                                })
                        })
                        .unwrap_or_else(|| capability.clone());
                    let history_arguments = arguments.clone();
                    let cap_call = CapabilityCall {
                        capability_id: capability.clone(),
                        capability_name,
                        arguments,
                    };
                    let outcome = match self.capability_service() {
                        Ok(Some(service)) => match service.execute_for_agent("agent", &cap_call) {
                            Ok(result) => {
                                if let Some(fail) = Self::output_indicates_failure(&result.output) {
                                    Err(format!("{capability}: {fail}"))
                                } else {
                                    let summary = crate::common::json_util::truncate_head_tail(
                                        &result.output.to_string(),
                                        4000,
                                    );
                                    Ok(summary)
                                }
                            }
                            Err(e) => Err(format!("{capability}: {e}")),
                        },
                        Ok(None) => Err("no capability runtime".to_string()),
                        Err(e) => Err(e),
                    };
                    tool_call_count += 1;
                    match outcome {
                        Ok(summary) => {
                            logs.push(format!("OK {capability}: {summary}"));
                            messages.push(ChatMessage::Assistant {
                                text: serde_json::json!({"tool_call": {"name": capability, "arguments": history_arguments}}).to_string(),
                                tool_calls: vec![],
                            });
                            messages.push(ChatMessage::User {
                                text: format!("能力 {capability} 执行结果: {summary}"),
                            });
                        }
                        Err(e) => {
                            logs.push(format!("FAIL {capability}: {e}"));
                            messages.push(ChatMessage::User {
                                text: format!(
                                    "能力 {capability} 执行失败: {e}\n分析错误并调整参数重试, 或输出 done 结束 (说明失败原因)"
                                ),
                            });
                        }
                    }
                }
                SubagentAction::Invalid(reason) => {
                    logs.push(format!("INVALID output (turn {turn}): {reason}"));
                    messages.push(ChatMessage::User {
                        text: format!(
                            "你的输出无法解析: {reason}\n只输出 JSON: {{\"arguments\": {{...}}}} 或 {{\"done\": true, \"summary\": \"...\"}}"
                        ),
                    });
                }
            }
        }
        logs.push(format!("EXCEEDED max_turns={max_turns}"));
        (
            NodeResult {
                node_id: node.id.clone(),
                status: NodeStatus::Failed,
                summary: String::new(),
                error: Some(format!("subagent exceeded max_turns={max_turns}")),
                tool_call_count,
                tool_call_logs: logs,
            },
            max_turns,
        )
    }

    fn capability_service(&self) -> std::result::Result<Option<CapabilityService<'_>>, String> {
        match (&self.registry, &self.executor) {
            (Some(registry), Some(executor)) => CapabilityService::new(registry, executor)
                .map(Some)
                .map_err(|e| format!("capability service init: {e}")),
            _ => Ok(None),
        }
    }

    fn output_indicates_failure(output: &serde_json::Value) -> Option<String> {
        ExecutionPlatform::output_indicates_failure(output)
    }
}

pub struct ExecutionPlatform {
    execution_rx: mpsc::Receiver<AgentMessage>,

    pool: Arc<AgentPool>,

    provider: Arc<dyn LlmProvider>,

    model_row: ModelRow,

    api_key: SecretString,

    subagent_pool: Arc<SubAgentPool>,

    trivium_db: Option<Arc<tokio::sync::Mutex<TriviumDb>>>,

    product_store: Option<Arc<PlatformProductStore>>,

    cursor_store: Option<Arc<CursorStore>>,

    prompts_dir: Option<PathBuf>,

    capability_ids: Vec<String>,

    registry: Option<Registry>,

    executor: Option<Arc<CapabilityExecutor>>,
}

/// 探针 (examples/exec_probe) 输出的设计阶段指标。
#[doc(hidden)]
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProbeDesignStats {
    pub parse_attempts: u32,
    pub parse_ok: bool,
    pub node_count: usize,
    pub error: Option<String>,
    pub kind: String,
}

/// 探针 (examples/exec_probe) 输出的单节点指标。
#[doc(hidden)]
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProbeNodeStats {
    pub node_id: String,
    pub capability: String,
    pub path: String,
    pub status: String,
    pub tool_calls: u32,
    pub turns: u32,
    pub duration_ms: u64,
    pub error: Option<String>,
    pub logs: Vec<String>,
}

/// 探针 (examples/exec_probe) 输出的 usage 汇总。
#[doc(hidden)]
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProbeUsage {
    pub prompt: u32,
    pub completion: u32,
}

/// 探针 (examples/exec_probe) 的整体运行报告。
#[doc(hidden)]
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProbeRunReport {
    pub goal: String,
    pub design: ProbeDesignStats,
    pub nodes: Vec<ProbeNodeStats>,
    pub ok: bool,
    pub total_duration_ms: u64,
    pub usage: Option<ProbeUsage>,
}

impl ExecutionPlatform {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        execution_rx: mpsc::Receiver<AgentMessage>,
        pool: Arc<AgentPool>,
        provider: Arc<dyn LlmProvider>,
        model_row: ModelRow,
        api_key: SecretString,
        subagent_pool: Arc<SubAgentPool>,
        trivium_db: Option<Arc<tokio::sync::Mutex<TriviumDb>>>,
        product_store: Option<Arc<PlatformProductStore>>,
        cursor_store: Option<Arc<CursorStore>>,
        prompts_dir: Option<PathBuf>,
        capability_ids: Vec<String>,
        registry: Option<Registry>,
        executor: Option<Arc<CapabilityExecutor>>,
    ) -> Self {
        Self {
            execution_rx,
            pool,
            provider,
            model_row,
            api_key,
            subagent_pool,
            trivium_db,
            product_store,
            cursor_store,
            prompts_dir,
            capability_ids,
            registry,
            executor,
        }
    }

    pub fn spawn(mut self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            tracing::info!("execution_platform: started, polling rx");

            while let Some(msg) = self.execution_rx.recv().await {
                let pending = self.execution_rx.len();
                self.pool
                    .update_platform_status(move |s| s.execution_pending = pending)
                    .await;
                match msg {
                    AgentMessage::Execute { turn_id } => {
                        tracing::debug!("execution_platform: received Execute({turn_id})");
                        self.pool
                            .update_platform_status(|s| s.execution_active = Some(turn_id.clone()))
                            .await;
                        self.handle_execute(&turn_id).await;
                        self.pool
                            .update_platform_status(|s| s.execution_active = None)
                            .await;
                    }
                    AgentMessage::Cancel { turn_id } => {
                        tracing::debug!("execution_platform: received Cancel({turn_id})");
                        self.handle_cancel(&turn_id).await;
                    }
                    other => {
                        tracing::warn!("execution_platform: unexpected message: {:?}", other);
                    }
                }

                self.pool.snapshot_detailed().await;
            }

            tracing::info!("execution_platform: rx closed, shutting down");
        })
    }

    async fn handle_execute(&mut self, turn_id: &str) {
        let ctx = match self.pool.get_turn_context(turn_id).await {
            Some(ctx) => ctx,
            None => {
                tracing::warn!("execution_platform: TurnContext not found for turn_id={turn_id}");
                return;
            }
        };

        if ctx.thinking.decision == ThinkDecision::Failure {
            self.write_thinking_failure(turn_id, &ctx.thinking.goal)
                .await;
            return;
        }

        tracing::debug!(
            "execution_platform: processing turn_id={turn_id}, goal='{}'",
            ctx.thinking.goal
        );

        let template_kind = infer_template_kind(&ctx.thinking.goal, &ctx.thinking.constraints);

        let prompt = build_execution_prompt(
            template_kind,
            &ctx.thinking.goal,
            &ctx.thinking.constraints,
            self.prompts_dir.as_deref(),
            &self.capability_ids,
            self.registry.as_ref(),
        );

        let prompt = self.enrich_prompt_with_environment(&prompt).await;

        let design = match self.call_llm_for_design(&prompt).await {
            Ok(d) => d,
            Err(first_error) => {
                tracing::warn!(
                    "execution_platform: 设计解析失败 (turn_id={turn_id}), 重试 1 次: {first_error}"
                );
                let retry_prompt = design_retry_prompt(&prompt, &first_error.to_string());
                match self.call_llm_for_design(&retry_prompt).await {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::error!(
                            "execution_platform: LLM call failed for turn_id={turn_id}: {e}"
                        );
                        self.write_failure(turn_id, &format!("LLM design failed: {e}"))
                            .await;
                        return;
                    }
                }
            }
        };

        let (dag, node_results) = match design {
            ExecutionDesign::Flow(flow) => {
                tracing::info!(
                    "execution_platform: flow design for turn_id={turn_id}: {} nodes",
                    flow.nodes.len()
                );

                if flow.trigger.is_some() {
                    let node_count = flow.nodes.len();
                    let trigger_desc = format!("{:?}", flow.trigger);
                    self.schedule_flow(flow);
                    let results = vec![NodeResult {
                        node_id: "scheduler".to_string(),
                        status: NodeStatus::Completed,
                        summary: format!(
                            "已注册调度: {trigger_desc} ({node_count} 节点, 条件满足后执行)"
                        ),
                        error: None,
                        tool_call_count: 0,
                        tool_call_logs: vec![format!("SCHEDULED: {trigger_desc}")],
                    }];
                    let dag = ExecutionDag::Dag { nodes: vec![] };
                    (dag, results)
                } else {
                    let results = self.execute_flow(&flow).await;
                    let dag = ExecutionDag::Dag {
                        nodes: flow
                            .nodes
                            .into_iter()
                            .map(|n| super::communication::DagNode {
                                id: n.id,
                                template_kind: "flow".to_string(),
                                capability_ids: vec![],
                                task_context: n.task_description,
                                depends_on: n.depends_on,
                            })
                            .collect(),
                    };
                    (dag, results)
                }
            }
            ExecutionDesign::Single(single) => {
                tracing::debug!(
                    "execution_platform: single subagent for turn_id={turn_id}: template_kind={}, capability_ids={:?}",
                    single.template_kind,
                    single.capability_ids
                );
                let node_result = self.dispatch_single_subagent(&single).await;
                let dag = ExecutionDag::Single {
                    template_kind: single.template_kind.clone(),
                    capability_ids: single.capability_ids.clone(),
                    task_context: single.task_context.clone(),
                };
                (dag, vec![node_result])
            }
            ExecutionDesign::Dag(dag_design) => {
                tracing::debug!(
                    "execution_platform: DAG multi-node for turn_id={turn_id}: {} nodes",
                    dag_design.nodes.len()
                );
                let results = self.execute_dag(&dag_design, turn_id).await;
                let dag = ExecutionDag::Dag {
                    nodes: dag_design
                        .nodes
                        .into_iter()
                        .map(|n| super::communication::DagNode {
                            id: n.id,
                            template_kind: "dag".to_string(),
                            capability_ids: n.capability_ids,
                            task_context: n.task_context,
                            depends_on: n.depends_on,
                        })
                        .collect(),
                };
                (dag, results)
            }
        };

        let execution_status = if node_results
            .iter()
            .all(|r| r.status == NodeStatus::Completed)
        {
            ExecutionStatus::Success
        } else if node_results
            .iter()
            .any(|r| r.status == NodeStatus::Completed)
        {
            ExecutionStatus::PartialFailure
        } else {
            ExecutionStatus::Failure
        };

        let output = ExecutionOutput {
            dag,
            node_results,
            status: execution_status,
        };

        self.pool.set_execution(turn_id, output.clone()).await;

        let occurred_at = UtcTimestamp::now();
        let thought_id = thought_id_from_turn(turn_id);
        if let Some(ref ps) = self.product_store {
            if let Err(e) = ps.write(ProductType::Execution, &thought_id, &occurred_at, &output) {
                tracing::warn!(
                    "execution_platform: failed to persist ExecutionProduct for {turn_id}: {e}"
                );
            }
        }
        if let Some(ref cs) = self.cursor_store {
            match cs.load("execution") {
                Ok(mut cursor) => {
                    cursor.advance(&occurred_at, std::slice::from_ref(&thought_id));
                    if let Err(e) = cs.save(&cursor) {
                        tracing::warn!(
                            "execution_platform: failed to advance execution cursor for {turn_id}: {e}"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "execution_platform: failed to load execution cursor for {turn_id}: {e}"
                    );
                }
            }
        }

        if let Err(e) = self.pool.send_execution_done(turn_id).await {
            tracing::warn!("execution_platform: send_execution_done failed: {e}");
        }
        if let Err(e) = self.pool.send_trigger(turn_id, "execution_complete").await {
            tracing::warn!("execution_platform: send_trigger execution_complete failed: {e}");
        }

        tracing::debug!("execution_platform: turn_id={turn_id} done, ExecutionDone DM sent");
    }

    async fn execute_dag(&self, dag: &DagDesign, turn_id: &str) -> Vec<NodeResult> {
        let order = match topological_sort(&dag.nodes) {
            Ok(o) => o,
            Err(e) => {
                tracing::error!(
                    "execution_platform: DAG cycle detected for turn_id={turn_id}: {e}"
                );

                return dag
                    .nodes
                    .iter()
                    .map(|n| NodeResult {
                        node_id: n.id.clone(),
                        status: NodeStatus::Failed,
                        summary: String::new(),
                        error: Some(format!("DAG cycle: {e}")),
                        tool_call_count: 0,
                        tool_call_logs: vec![format!("DAG cycle detected: {e}")],
                    })
                    .collect();
            }
        };

        let node_map: HashMap<&str, &DagNodeDesign> =
            dag.nodes.iter().map(|n| (n.id.as_str(), n)).collect();

        let mut handle_map: HashMap<String, String> = HashMap::new();
        for node in &dag.nodes {
            let handle =
                self.subagent_pool
                    .spawn(&node.id, &node.task_context, node.capability_ids.clone());
            handle_map.insert(node.id.clone(), handle.id);
        }

        for node_id in &order {
            let node = node_map[node_id.as_str()];
            let subagent_id = &handle_map[node_id];

            let all_deps_ok = node.depends_on.iter().all(|dep| {
                if let Some(dep_sub_id) = handle_map.get(dep) {
                    if let Some(inst) = self.subagent_pool.get(dep_sub_id) {
                        return inst.status == SubAgentStatus::Completed;
                    }
                }
                false
            });

            if !all_deps_ok {
                self.subagent_pool
                    .mark_failed(subagent_id, "dependency failed or not completed");
                self.subagent_pool
                    .append_log(subagent_id, "SKIPPED: dependency not satisfied");
                continue;
            }

            self.subagent_pool
                .update_status(subagent_id, SubAgentStatus::Running);
            self.subagent_pool.append_log(
                subagent_id,
                &format!(
                    "STARTED: node_id={}, task_context={}",
                    node.id, node.task_context
                ),
            );

            let result = self.execute_subagent_node(subagent_id, node).await;

            match result {
                Ok(logs) => {
                    self.subagent_pool
                        .mark_completed(subagent_id, logs.tool_call_count);
                    for log_line in &logs.lines {
                        self.subagent_pool.append_log(subagent_id, log_line);
                    }
                }
                Err(e) => {
                    self.subagent_pool.mark_failed(subagent_id, &e);
                    self.subagent_pool
                        .append_log(subagent_id, &format!("FAILED: {e}"));
                }
            }

            if let Some(ref db_lock) = self.trivium_db {
                let mut db = db_lock.lock().await;
                let inst = self.subagent_pool.get(subagent_id);
                if let Some(inst) = inst {
                    let log_content = inst.logs.join("\n");
                    let _ = insert_raw_file_node(
                        &mut db,
                        &format!("exec:{turn_id}:{}", node.id),
                        &format!("execution_logs/{turn_id}/{}.log", node.id),
                        "text/plain",
                        log_content.len() as u64,
                        &format!("execution:{}", turn_id),
                    );
                }
            }
        }

        self.subagent_pool.collect_results()
    }

    async fn execute_flow(&self, flow: &TaskFlow) -> Vec<NodeResult> {
        let layers = match topological_layers(&flow.nodes) {
            Ok(l) => l,
            Err(e) => {
                tracing::error!("execution_platform: flow cycle detected: {e}");
                return flow
                    .nodes
                    .iter()
                    .map(|n| NodeResult {
                        node_id: n.id.clone(),
                        status: NodeStatus::Failed,
                        summary: String::new(),
                        error: Some(format!("flow cycle: {e}")),
                        tool_call_count: 0,
                        tool_call_logs: vec![],
                    })
                    .collect();
            }
        };
        let by_id: HashMap<String, &TaskNode> =
            flow.nodes.iter().map(|n| (n.id.clone(), n)).collect();
        let mut results: HashMap<String, NodeResult> = HashMap::new();
        let mut node_results = Vec::new();

        for (layer_index, layer) in layers.iter().enumerate() {
            tracing::info!(
                "execution_platform: flow layer {} ({} nodes, parallel)",
                layer_index,
                layer.len()
            );

            let mut join_set = tokio::task::JoinSet::new();
            for node_id in layer {
                let node = by_id[node_id.as_str()];

                let failed_dep = node.depends_on.iter().find(|dep| {
                    results
                        .get(*dep)
                        .is_some_and(|r| r.status != NodeStatus::Completed)
                });
                if let Some(dep) = failed_dep {
                    node_results.push(NodeResult {
                        node_id: node.id.clone(),
                        status: NodeStatus::Skipped,
                        summary: String::new(),
                        error: Some(format!("dependency '{dep}' failed/skipped")),
                        tool_call_count: 0,
                        tool_call_logs: vec![format!("SKIPPED: dependency '{dep}' not completed")],
                    });
                    results.insert(
                        node.id.clone(),
                        NodeResult {
                            node_id: node.id.clone(),
                            status: NodeStatus::Skipped,
                            summary: String::new(),
                            error: Some(format!("dependency '{dep}' failed/skipped")),
                            tool_call_count: 0,
                            tool_call_logs: vec![],
                        },
                    );
                    continue;
                }

                let dep_summary = build_dep_summary(node, &results, &by_id);
                let node_owned = node.clone();
                let runner = self.node_runner();
                if let Some(args) = node.prefilled_arguments.clone() {
                    match prefilled_arguments_oversized(&args) {
                        Some(bytes) => {
                            tracing::warn!(
                                "execution_platform: prefilled degraded: oversized ({bytes} bytes > {PREFILLED_MAX_BYTES}), node '{}' 改走 subagent",
                                node.id
                            );
                            join_set.spawn(async move {
                                runner.run_subagent_loop(&node_owned, &dep_summary).await.0
                            });
                        }
                        None => {
                            join_set.spawn(async move {
                                runner.execute_prefilled_node(&node_owned, &args).await
                            });
                        }
                    }
                } else {
                    join_set.spawn(async move {
                        runner.run_subagent_loop(&node_owned, &dep_summary).await.0
                    });
                }
            }

            while let Some(outcome) = join_set.join_next().await {
                match outcome {
                    Ok(result) => {
                        results.insert(result.node_id.clone(), result.clone());
                        node_results.push(result);
                    }
                    Err(join_error) => {
                        tracing::error!("execution_platform: node task panicked: {join_error}");
                    }
                }
            }
        }
        node_results
    }

    fn node_runner(&self) -> NodeRunner {
        NodeRunner {
            registry: self.registry.clone(),
            executor: self.executor.clone(),
            provider: self.provider.clone(),
            model_row: self.model_row.clone(),
            api_key: self.api_key.clone(),
        }
    }

    fn schedule_flow(&self, flow: TaskFlow) {
        let runner = self.node_runner();
        let pool = self.pool.clone();
        match flow.trigger.clone() {
            Some(TriggerSpec::Event { kind }) => {
                tracing::info!(
                    "execution_platform: schedule event trigger kind={kind}, {} nodes",
                    flow.nodes.len()
                );

                let mut rx = pool.subscribe_events();
                let event_kind = kind.clone();
                tokio::spawn(async move {
                    tracing::info!("scheduler: event trigger '{event_kind}' armed (broadcast)");
                    loop {
                        match rx.recv().await {
                            Ok(event) if event.kind == event_kind => {
                                tracing::info!(
                                    "scheduler: event '{event_kind}' fired (detail={}), executing TaskFlow",
                                    event.detail
                                );
                                let results = runner.execute_flow_public(&flow).await;
                                tracing::info!(
                                    "scheduler: event '{event_kind}' done, {} node results",
                                    results.len()
                                );
                            }
                            Ok(_) => {}
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                tracing::warn!(
                                    "scheduler: event bus lagged, skipping missed events"
                                );
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                tracing::warn!(
                                    "scheduler: event bus closed, event trigger stopping"
                                );
                                break;
                            }
                        }
                    }
                });
            }
            Some(TriggerSpec::Webhook { url }) => {
                tracing::info!(
                    "execution_platform: schedule webhook trigger {url}, {} nodes",
                    flow.nodes.len()
                );

                match webhook_listen_addr(&url) {
                    Some(addr) => {
                        let runner_for_hook = runner.clone();
                        tokio::spawn(async move {
                            let listener = match std::net::TcpListener::bind(&addr) {
                                Ok(l) => l,
                                Err(e) => {
                                    tracing::error!("scheduler: webhook bind {addr} failed: {e}");
                                    return;
                                }
                            };
                            tracing::info!("scheduler: webhook listening on {addr}");
                            loop {
                                match listener.accept() {
                                    Ok((mut stream, peer)) => {
                                        use std::io::{Read, Write};
                                        tracing::info!(
                                            "scheduler: webhook request from {peer}, executing TaskFlow"
                                        );

                                        let mut buf = [0u8; 4096];
                                        let _ = stream.read(&mut buf);
                                        let _ = stream.write_all(
                                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK",
                                        );
                                        let _ = stream.flush();
                                        drop(stream);
                                        let results =
                                            runner_for_hook.execute_flow_public(&flow).await;
                                        tracing::info!(
                                            "scheduler: webhook done, {} node results",
                                            results.len()
                                        );
                                    }
                                    Err(e) => {
                                        tracing::warn!("scheduler: webhook accept failed: {e}");
                                    }
                                }
                            }
                        });
                    }
                    None => {
                        tracing::warn!(
                            "execution_platform: webhook url 解析失败: {url}, 已跳过注册"
                        );
                    }
                }
            }
            Some(TriggerSpec::Cron { schedule }) => {
                let secs = parse_schedule_to_seconds(&schedule).unwrap_or(60);
                tracing::info!(
                    "execution_platform: schedule interval {secs}s ({schedule}), {} nodes",
                    flow.nodes.len()
                );
                tokio::spawn(async move {
                    let mut interval =
                        tokio::time::interval(std::time::Duration::from_secs(secs.max(1)));
                    loop {
                        interval.tick().await;
                        tracing::info!("scheduler: interval {secs}s firing TaskFlow");
                        let results = runner.execute_flow_public(&flow).await;
                        tracing::info!("scheduler: interval fired, {} node results", results.len());
                    }
                });
            }
            None => {}
        }
    }

    async fn enrich_prompt_with_environment(&self, prompt: &str) -> String {
        let snapshot = self.pool.snapshot_detailed().await;
        let agent_lines: Vec<String> = if snapshot.entries.is_empty() {
            vec!["(无运行中 agent)".to_string()]
        } else {
            snapshot
                .entries
                .iter()
                .map(|e| format!("- {:?}: {:?}", e.identity, e.status))
                .collect()
        };

        let ws_root = self
            .executor
            .as_ref()
            .and_then(|_| std::env::current_dir().ok());
        let mut workspace_lines: Vec<String> = Vec::new();
        if let Some(root) = ws_root {
            if let Ok(rd) = std::fs::read_dir(&root) {
                for entry in rd.flatten().take(20) {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        workspace_lines.push(format!("- {name}/"));

                        if let Ok(sub) = std::fs::read_dir(entry.path()) {
                            let subs: Vec<String> = sub
                                .flatten()
                                .take(8)
                                .map(|s| format!("  - {name}/{}", s.file_name().to_string_lossy()))
                                .collect();
                            workspace_lines.extend(subs);
                        }
                    } else {
                        workspace_lines.push(format!("- {name}"));
                    }
                }
            }
        }
        if workspace_lines.is_empty() {
            workspace_lines.push("(工作区为空)".to_string());
        }

        format!(
            "{}\n\n## Environment Context\n\n**Agent 池快照:**\n{}\n\n**工作区文件清单:**\n{}",
            prompt,
            agent_lines.join("\n"),
            workspace_lines.join("\n")
        )
    }

    async fn execute_subagent_node(
        &self,
        subagent_id: &str,
        node: &DagNodeDesign,
    ) -> std::result::Result<SubAgentLogs, String> {
        tracing::info!(
            "execution_platform: executing subagent {} (node_id={}, capabilities={:?})",
            subagent_id,
            node.id,
            node.capability_ids
        );

        let (registry, executor) = match (&self.registry, &self.executor) {
            (Some(registry), Some(executor)) => {
                (Some(registry.clone()), Some(Arc::clone(executor)))
            }
            _ => (None, None),
        };
        let Some((registry, executor)) = registry.zip(executor) else {
            let mut lines = vec![
                format!("node_id: {}", node.id),
                format!("task_context: {}", node.task_context),
                format!("capabilities: {:?}", node.capability_ids),
                format!("timeout: {}s", node.timeout_seconds),
            ];
            lines.push("NO_RUNTIME: registry/executor 未配置, 降级模拟".to_string());
            lines.push("EXECUTED: simulation completed (no runtime)".to_string());
            return Ok(SubAgentLogs {
                lines,
                tool_call_count: 0,
            });
        };

        let node_id = node.id.clone();
        let caps = node.capability_ids.clone();
        let task_ctx = node.task_context.clone();
        let args = node.arguments.clone();
        let timeout_secs = node.timeout_seconds.max(1) as u64;

        let handle = tokio::task::spawn_blocking(move || {
            let service = CapabilityService::new(&registry, &executor)
                .map_err(|e| format!("capability service init: {e}"))?;
            let mut lines = vec![
                format!("node_id: {node_id}"),
                format!("task_context: {task_ctx}"),
                format!("capabilities: {:?}", caps),
                format!("timeout: {timeout_secs}s"),
            ];
            let mut tool_call_count = 0u32;
            for cap_id in &caps {
                let name = registry
                    .base_capabilities
                    .get(cap_id)
                    .map(|row| row.name.clone())
                    .or_else(|| {
                        registry
                            .composite_capabilities
                            .get(cap_id)
                            .map(|row| row.name.clone())
                    })
                    .unwrap_or_else(|| cap_id.to_string());
                let arguments = Self::design_args_for(args.as_ref(), cap_id)
                    .filter(|v| v.is_object())
                    .cloned()
                    .unwrap_or_else(|| parse_task_context(cap_id, &task_ctx));
                let call = CapabilityCall {
                    capability_id: cap_id.clone(),
                    capability_name: name,
                    arguments,
                };
                match service.execute_for_agent("agent", &call) {
                    Ok(result) => {
                        if let Some(fail_reason) = Self::output_indicates_failure(&result.output) {
                            lines.push(format!("FAIL {cap_id}: {fail_reason}"));
                            return Err(format!("{cap_id}: {fail_reason}"));
                        }
                        tool_call_count += 1;
                        let preview = result.output.to_string();
                        let preview: String = preview.chars().take(200).collect();
                        lines.push(format!("OK {cap_id}: {preview}"));
                    }
                    Err(e) => {
                        lines.push(format!("FAIL {cap_id}: {e}"));
                        return Err(format!("{cap_id}: {e}"));
                    }
                }
            }
            lines.push(format!("EXECUTED: {tool_call_count} capability call(s)"));
            Ok((lines, tool_call_count))
        });

        match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), handle).await {
            Ok(Ok(Ok((lines, tool_call_count)))) => Ok(SubAgentLogs {
                lines,
                tool_call_count,
            }),
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(join_error)) => Err(format!("subagent node task panicked: {join_error}")),
            Err(_) => Err(format!("node {} timed out after {timeout_secs}s", node.id)),
        }
    }

    fn capability_service(&self) -> std::result::Result<Option<CapabilityService<'_>>, String> {
        match (&self.registry, &self.executor) {
            (Some(registry), Some(executor)) => CapabilityService::new(registry, executor)
                .map(Some)
                .map_err(|e| format!("capability service init: {e}")),
            _ => Ok(None),
        }
    }

    #[allow(dead_code)]
    fn build_capability_call(&self, capability_id: &str, task_context: &str) -> CapabilityCall {
        self.build_capability_call_with_args(capability_id, task_context, None)
    }

    fn build_capability_call_with_args(
        &self,
        capability_id: &str,
        task_context: &str,
        explicit_args: Option<&serde_json::Value>,
    ) -> CapabilityCall {
        let name = self
            .registry
            .as_ref()
            .and_then(|r| {
                r.base_capabilities
                    .get(capability_id)
                    .map(|row| &row.name)
                    .or_else(|| {
                        r.composite_capabilities
                            .get(capability_id)
                            .map(|row| &row.name)
                    })
            })
            .cloned()
            .unwrap_or_else(|| capability_id.to_string());
        let arguments = explicit_args
            .filter(|v| v.is_object())
            .cloned()
            .unwrap_or_else(|| parse_task_context(capability_id, task_context));
        CapabilityCall {
            capability_id: capability_id.to_string(),
            capability_name: name,
            arguments,
        }
    }

    fn output_indicates_failure(output: &serde_json::Value) -> Option<String> {
        if output.get("success").and_then(|v| v.as_bool()) == Some(false) {
            let msg = output
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Some(msg.to_string());
        }
        if let Some(err) = output.get("error").and_then(|v| v.as_str()) {
            return Some(err.to_string());
        }
        let code = output.get("exit_code").and_then(|v| v.as_i64())?;
        if code == 0 {
            return None;
        }
        let stderr = output.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
        let stderr: String = stderr.chars().take(120).collect();
        Some(format!("exit_code={code} stderr={stderr}"))
    }

    fn design_args_for<'a>(
        arguments: Option<&'a serde_json::Value>,
        capability_id: &str,
    ) -> Option<&'a serde_json::Value> {
        arguments
            .and_then(|v| v.as_object())
            .and_then(|obj| obj.get(capability_id))
            .filter(|v| v.is_object())
    }

    async fn handle_cancel(&self, turn_id: &str) {
        self.pool.cancel_turn(turn_id).await;
        tracing::debug!("execution_platform: turn_id={turn_id} cancelled");
    }

    async fn call_llm_for_design(&self, prompt: &str) -> Result<ExecutionDesign> {
        let messages = vec![
            ChatMessage::System {
                text: prompt.to_string(),
                kind: SystemKind::Primary,
            },
            ChatMessage::User {
                text: "Design the execution plan now. Output ONLY the JSON.".to_string(),
            },
        ];

        let req = LlmRequest::from_model_row(&self.model_row, messages, self.api_key.clone())?;

        let resp = self.provider.call(&req).await?;

        parse_execution_design(&resp.content)
    }

    /// 探针 (examples/exec_probe) 最小入口: 设计→执行全链路, 返回机器可读指标。
    /// 不触发 DM/洞察/记忆; 直接走 TaskFlow 设计→节点执行路径。
    #[doc(hidden)]
    pub async fn probe_goal(&self, goal: &str) -> ProbeRunReport {
        let started = std::time::Instant::now();
        let template_kind = infer_template_kind(goal, &[]);
        let base_prompt = build_execution_prompt(
            template_kind,
            goal,
            &[],
            self.prompts_dir.as_deref(),
            &self.capability_ids,
            self.registry.as_ref(),
        );
        let prompt = self.enrich_prompt_with_environment(&base_prompt).await;

        let mut parse_attempts = 0u32;
        let mut design_outcome: Result<ExecutionDesign> = Err(AgentError::Parse(
            "probe: design LLM call did not run".to_string(),
        ));
        let mut usage: Option<ProbeUsage> = None;

        for round in 0..2u32 {
            let round_prompt = if round == 0 {
                prompt.clone()
            } else {
                let err = match &design_outcome {
                    Err(e) => e.to_string(),
                    Ok(_) => break,
                };
                design_retry_prompt(&prompt, &err)
            };
            match self.probe_call_for_design(&round_prompt).await {
                Ok((design, attempts, round_usage)) => {
                    parse_attempts += attempts;
                    if let Some(u) = round_usage {
                        usage = Some(ProbeUsage {
                            prompt: u.prompt_tokens,
                            completion: u.completion_tokens,
                        });
                    }
                    design_outcome = Ok(design);
                    break;
                }
                Err((parse_err, attempts, round_usage)) => {
                    parse_attempts += attempts;
                    if let Some(u) = round_usage {
                        usage = Some(ProbeUsage {
                            prompt: u.prompt_tokens,
                            completion: u.completion_tokens,
                        });
                    }
                    design_outcome = Err(parse_err);
                }
            }
        }

        let design_report = match &design_outcome {
            Ok(ExecutionDesign::Flow(flow)) => ProbeDesignStats {
                parse_attempts,
                parse_ok: true,
                node_count: flow.nodes.len(),
                error: None,
                kind: "flow".to_string(),
            },
            Ok(ExecutionDesign::Single(_single)) => ProbeDesignStats {
                parse_attempts,
                parse_ok: true,
                node_count: 1,
                error: None,
                kind: "single".to_string(),
            },
            Ok(ExecutionDesign::Dag(dag)) => ProbeDesignStats {
                parse_attempts,
                parse_ok: true,
                node_count: dag.nodes.len(),
                error: None,
                kind: "dag".to_string(),
            },
            Err(e) => ProbeDesignStats {
                parse_attempts,
                parse_ok: false,
                node_count: 0,
                error: Some(e.to_string()),
                kind: "none".to_string(),
            },
        };

        let mut nodes: Vec<ProbeNodeStats> = Vec::new();
        if let Ok(ExecutionDesign::Flow(flow)) = &design_outcome {
            nodes = self.probe_execute_flow(flow).await;
        } else if let Ok(ExecutionDesign::Single(single)) = &design_outcome {
            let start = std::time::Instant::now();
            let capability = single.capability_ids.first().cloned().unwrap_or_default();
            let result = self.dispatch_single_subagent(single).await;
            nodes.push(ProbeNodeStats {
                node_id: "subagent-1".to_string(),
                capability,
                path: "subagent".to_string(),
                status: format!("{:?}", result.status),
                tool_calls: result.tool_call_count,
                turns: 1,
                duration_ms: start.elapsed().as_millis() as u64,
                error: result.error.clone(),
                logs: result.tool_call_logs.clone(),
            });
        }

        let ok = design_report.parse_ok
            && nodes
                .iter()
                .all(|n| n.status == format!("{:?}", NodeStatus::Completed));

        ProbeRunReport {
            goal: goal.to_string(),
            design: design_report,
            nodes,
            ok,
            total_duration_ms: started.elapsed().as_millis() as u64,
            usage,
        }
    }

    /// 探针用的设计 LLM 调用: 返回 (设计, 解析尝试次数, usage)。
    /// 失败时返回 Err((解析错误, 尝试次数, usage))。
    async fn probe_call_for_design(
        &self,
        prompt: &str,
    ) -> std::result::Result<
        (
            ExecutionDesign,
            u32,
            Option<crate::logic::model::provider::Usage>,
        ),
        (
            AgentError,
            u32,
            Option<crate::logic::model::provider::Usage>,
        ),
    > {
        let messages = vec![
            ChatMessage::System {
                text: prompt.to_string(),
                kind: SystemKind::Primary,
            },
            ChatMessage::User {
                text: "Design the execution plan now. Output ONLY the JSON.".to_string(),
            },
        ];
        let req = match LlmRequest::from_model_row(&self.model_row, messages, self.api_key.clone())
        {
            Ok(r) => r,
            Err(e) => return Err((e, 0, None)),
        };
        let resp = match self.provider.call(&req).await {
            Ok(r) => r,
            Err(e) => return Err((e, 0, None)),
        };
        let (parsed, attempts) = probe_parse_execution_design(&resp.content);
        match parsed {
            Ok(design) => Ok((design, attempts, resp.usage)),
            Err(e) => Err((e, attempts, resp.usage)),
        }
    }

    /// 探针用的串行流式执行: 逐层逐节点执行并记录指标 (path/turns/duration)。
    async fn probe_execute_flow(&self, flow: &TaskFlow) -> Vec<ProbeNodeStats> {
        let layers = match topological_layers(&flow.nodes) {
            Ok(l) => l,
            Err(e) => {
                tracing::error!("probe: flow cycle detected: {e}");
                return flow
                    .nodes
                    .iter()
                    .map(|n| ProbeNodeStats {
                        node_id: n.id.clone(),
                        capability: n.capability.clone(),
                        path: "skipped".to_string(),
                        status: format!("{:?}", NodeStatus::Failed),
                        tool_calls: 0,
                        turns: 0,
                        duration_ms: 0,
                        error: Some(format!("flow cycle: {e}")),
                        logs: vec![],
                    })
                    .collect();
            }
        };
        let by_id: HashMap<String, &TaskNode> =
            flow.nodes.iter().map(|n| (n.id.clone(), n)).collect();
        let mut results: HashMap<String, NodeResult> = HashMap::new();
        let mut node_stats: Vec<ProbeNodeStats> = Vec::new();

        for layer in &layers {
            for node_id in layer {
                let node = by_id[node_id.as_str()];
                let failed_dep = node.depends_on.iter().find(|dep| {
                    results
                        .get(*dep)
                        .is_some_and(|r| r.status != NodeStatus::Completed)
                });
                if let Some(dep) = failed_dep {
                    node_stats.push(ProbeNodeStats {
                        node_id: node.id.clone(),
                        capability: node.capability.clone(),
                        path: "skipped".to_string(),
                        status: format!("{:?}", NodeStatus::Skipped),
                        tool_calls: 0,
                        turns: 0,
                        duration_ms: 0,
                        error: Some(format!("dependency '{dep}' failed/skipped")),
                        logs: vec![],
                    });
                    results.insert(
                        node.id.clone(),
                        NodeResult {
                            node_id: node.id.clone(),
                            status: NodeStatus::Skipped,
                            summary: String::new(),
                            error: Some(format!("dependency '{dep}' failed/skipped")),
                            tool_call_count: 0,
                            tool_call_logs: vec![],
                        },
                    );
                    continue;
                }

                let dep_summary = build_dep_summary(node, &results, &by_id);
                let runner = self.node_runner();
                let start = std::time::Instant::now();
                let (result, turns, path) = if let Some(args) = node.prefilled_arguments.clone() {
                    match prefilled_arguments_oversized(&args) {
                        Some(bytes) => {
                            tracing::warn!(
                                    "probe: prefilled degraded: oversized ({bytes} bytes > {PREFILLED_MAX_BYTES}), node '{}' 改走 subagent",
                                    node.id
                                );
                            let (r, t) = runner.run_subagent_loop(node, &dep_summary).await;
                            (r, t, "degraded_subagent")
                        }
                        None => {
                            let r = runner.execute_prefilled_node(node, &args).await;
                            (r, 1, "prefilled")
                        }
                    }
                } else {
                    let (r, t) = runner.run_subagent_loop(node, &dep_summary).await;
                    (r, t, "subagent")
                };
                node_stats.push(ProbeNodeStats {
                    node_id: node.id.clone(),
                    capability: node.capability.clone(),
                    path: path.to_string(),
                    status: format!("{:?}", result.status),
                    tool_calls: result.tool_call_count,
                    turns,
                    duration_ms: start.elapsed().as_millis() as u64,
                    error: result.error.clone(),
                    logs: result.tool_call_logs.clone(),
                });
                results.insert(result.node_id.clone(), result);
            }
        }
        node_stats
    }

    async fn dispatch_single_subagent(&self, design: &SubAgentDesign) -> NodeResult {
        tracing::info!(
            "execution_platform: dispatching subagent: template_kind={}, capabilities={:?}, max_turns={}, timeout={}s",
            design.template_kind,
            design.capability_ids,
            design.max_turns,
            design.timeout_seconds,
        );

        let handle = self.subagent_pool.spawn(
            "subagent-1",
            &design.task_context,
            design.capability_ids.clone(),
        );

        self.subagent_pool
            .update_status(&handle.id, SubAgentStatus::Running);

        self.subagent_pool.append_log(
            &handle.id,
            &format!("template_kind: {}", design.template_kind),
        );
        self.subagent_pool.append_log(
            &handle.id,
            &format!("capabilities: {:?}", design.capability_ids),
        );
        self.subagent_pool.append_log(
            &handle.id,
            &format!("task_context: {}", design.task_context),
        );
        self.subagent_pool.append_log(
            &handle.id,
            &format!(
                "max_turns: {}, timeout: {}s",
                design.max_turns, design.timeout_seconds
            ),
        );
        self.subagent_pool.append_log(
            &handle.id,
            &format!(
                "subagent designed: {} with capabilities {:?}",
                design.task_context, design.capability_ids
            ),
        );

        let mut tool_call_count = 0u32;
        let mut failure: Option<String> = None;
        match self.capability_service() {
            Ok(Some(service)) => {
                for cap_id in &design.capability_ids {
                    let call = self.build_capability_call_with_args(
                        cap_id,
                        &design.task_context,
                        Self::design_args_for(design.arguments.as_ref(), cap_id),
                    );
                    match service.execute_for_agent("agent", &call) {
                        Ok(result) => {
                            if let Some(fail_reason) =
                                Self::output_indicates_failure(&result.output)
                            {
                                tracing::warn!(
                                    "execution_platform: capability FAIL {cap_id}: {fail_reason}"
                                );
                                self.subagent_pool.append_log(
                                    &handle.id,
                                    &format!("FAIL {cap_id}: {fail_reason}"),
                                );
                                failure = Some(format!("{cap_id}: {fail_reason}"));
                                break;
                            }
                            tool_call_count += 1;
                            let preview = result.output.to_string();
                            let preview: String = preview.chars().take(200).collect();
                            tracing::info!("execution_platform: capability OK {cap_id}: {preview}");
                            self.subagent_pool
                                .append_log(&handle.id, &format!("OK {cap_id}: {preview}"));
                        }
                        Err(e) => {
                            tracing::warn!("execution_platform: capability FAIL {cap_id}: {e}");
                            self.subagent_pool
                                .append_log(&handle.id, &format!("FAIL {cap_id}: {e}"));
                            failure = Some(format!("{cap_id}: {e}"));
                            break;
                        }
                    }
                }
            }
            Ok(None) => {
                tracing::warn!("execution_platform: registry/executor 未配置, SubAgent 降级模拟");
                self.subagent_pool
                    .append_log(&handle.id, "NO_RUNTIME: registry/executor 未配置, 降级模拟");
            }
            Err(e) => {
                tracing::warn!("execution_platform: capability service init failed: {e}");
                failure = Some(e);
            }
        }

        match &failure {
            Some(e) => {
                self.subagent_pool.mark_failed(&handle.id, e);
            }
            None => {
                self.subagent_pool
                    .mark_completed(&handle.id, tool_call_count);
            }
        }

        let inst = self
            .subagent_pool
            .get(&handle.id)
            .unwrap_or_else(|| SubAgentInstance {
                id: handle.id.clone(),
                node_id: "subagent-1".to_string(),
                task_context: design.task_context.clone(),
                capability_ids: design.capability_ids.clone(),
                status: SubAgentStatus::Failed,
                logs: vec!["subagent instance not found after dispatch".to_string()],
                tool_call_count: 0,
                error: Some("subagent instance not found after dispatch".to_string()),
            });
        inst.into_node_result()
    }

    async fn write_failure(&self, turn_id: &str, error_msg: &str) {
        let output = ExecutionOutput {
            dag: ExecutionDag::Single {
                template_kind: "normal".to_string(),
                capability_ids: vec![],
                task_context: "execution failed".to_string(),
            },
            node_results: vec![NodeResult {
                node_id: "subagent-1".to_string(),
                status: NodeStatus::Failed,
                summary: String::new(),
                error: Some(error_msg.to_string()),
                tool_call_count: 0,
                tool_call_logs: vec![format!(
                    "execution_platform: LLM design failed: {error_msg}"
                )],
            }],
            status: ExecutionStatus::Failure,
        };

        self.pool.set_execution(turn_id, output).await;
        if let Err(e) = self.pool.send_execution_done(turn_id).await {
            tracing::warn!("execution_platform: send_execution_done failed: {e}");
        }
        if let Err(e) = self.pool.send_trigger(turn_id, "execution_complete").await {
            tracing::warn!("execution_platform: send_trigger execution_complete failed: {e}");
        }

        let occurred_at = UtcTimestamp::now();
        let thought_id = thought_id_from_turn(turn_id);
        let product = serde_json::json!({
            "state": "Failed",
            "error": error_msg,
            "turn_id": turn_id,
        });
        if let Some(ref ps) = self.product_store {
            if let Err(e) = ps.write(ProductType::Execution, &thought_id, &occurred_at, &product) {
                tracing::warn!(
                    "execution_platform: failed to persist Failed ExecutionProduct for {turn_id}: {e}"
                );
            }
        }
        if let Some(ref cs) = self.cursor_store {
            match cs.load("execution") {
                Ok(mut cursor) => {
                    cursor.advance(&occurred_at, std::slice::from_ref(&thought_id));
                    if let Err(e) = cs.save(&cursor) {
                        tracing::warn!(
                            "execution_platform: failed to advance execution cursor for {turn_id}: {e}"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "execution_platform: failed to load execution cursor for {turn_id}: {e}"
                    );
                }
            }
        }

        tracing::debug!(
            "execution_platform: turn_id={turn_id} failed, ExecutionDone sent with Failure status"
        );
    }

    async fn write_thinking_failure(&self, turn_id: &str, failure_payload: &str) {
        let output = ExecutionOutput {
            dag: ExecutionDag::Single {
                template_kind: "thinking_failure".to_string(),
                capability_ids: vec![],
                task_context: "thinking output validation failed".to_string(),
            },
            node_results: vec![NodeResult {
                node_id: "thinking-output-validation".to_string(),
                status: NodeStatus::Failed,
                summary:
                    "Thinking output was rejected before execution; no subagent was dispatched"
                        .to_string(),
                error: Some(failure_payload.to_string()),
                tool_call_count: 0,
                tool_call_logs: vec![],
            }],
            status: ExecutionStatus::Failure,
        };

        self.pool.set_execution(turn_id, output).await;
        if let Err(e) = self.pool.send_execution_done(turn_id).await {
            tracing::warn!("execution_platform: send_execution_done failed: {e}");
        }
        if let Err(e) = self.pool.send_trigger(turn_id, "execution_complete").await {
            tracing::warn!("execution_platform: send_trigger execution_complete failed: {e}");
        }
    }
}

struct SubAgentLogs {
    lines: Vec<String>,
    tool_call_count: u32,
}

fn parse_task_context(capability_id: &str, task_context: &str) -> serde_json::Value {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(task_context.trim()) {
        if v.is_object() {
            return v;
        }
    }
    let key = match capability_id {
        id if id.starts_with("file.") => "path",
        "shell.exec" => "command",
        "code.exec" => "code",
        "text.grep" => "path",
        "db.query" => "table",
        _ => "input",
    };
    serde_json::json!({ key: task_context })
}

fn infer_template_kind(goal: &str, constraints: &[String]) -> &'static str {
    let combined = {
        let mut s = goal.to_lowercase();
        for c in constraints {
            s.push(' ');
            s.push_str(&c.to_lowercase());
        }
        s
    };

    if combined.contains("dag") || combined.contains("multi-step") || combined.contains("parallel")
    {
        return "dag";
    }
    if combined.contains("trigger") || combined.contains("webhook") || combined.contains("event") {
        return "triggered";
    }
    if combined.contains("schedul") || combined.contains("cron") || combined.contains("periodic") {
        return "scheduled";
    }
    "normal"
}

fn build_execution_prompt(
    template_kind: &str,
    goal: &str,
    constraints: &[String],
    prompts_dir: Option<&Path>,
    capabilities: &[String],
    registry: Option<&Registry>,
) -> String {
    let base = if let Some(dir) = prompts_dir {
        select_prompt(template_kind, dir)
    } else {
        format!("You are the Execution Platform. Design a sub-agent for task: {goal}")
    };

    let constraints_str = if constraints.is_empty() {
        "none".to_string()
    } else {
        constraints
            .iter()
            .enumerate()
            .map(|(i, c)| format!("  {}. {}", i + 1, c))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let caps_str = if capabilities.is_empty() {
        "none".to_string()
    } else {
        capabilities
            .iter()
            .map(
                |c| match registry.and_then(|r| r.base_capabilities.get(c)) {
                    Some(row) => {
                        let desc = if row.description.is_empty() {
                            String::new()
                        } else {
                            format!("描述: {}", row.description)
                        };
                        let meta = row
                            .metadata
                            .as_ref()
                            .map(|m| format!(" 元数据: {}", m))
                            .unwrap_or_default();
                        format!("- {c}: {desc}{meta}\n  参数 schema {}", row.schema_in)
                    }
                    None => format!("- {c}"),
                },
            )
            .collect::<Vec<_>>()
            .join("\n")
    };

    format!(
        "{}\n\n## Task Input\n\n**Goal:** {}\n\n**Constraints:**\n{}\n\n**Available Capabilities:**\n{}\n\n**Template Kind:** {}\n\n## Output Format (TaskFlow v2)\n\
         输出**任务流程** JSON (节点-任务流程化设计, 一个节点一件事一种工具, **每个节点必须配 capability**):\n\n\
         示例 1 (一步执行):\n\
         ```json\n\
         {{\"template_kind\": \"normal\", \"nodes\": [\n\
           {{\"id\": \"n1\", \"depends_on\": [], \"task_description\": \"读 sales.csv\", \"expected_output\": \"csv 内容\",\n\
             \"capability\": \"file.read\", \"prefilled_arguments\": {{\"path\": \"sales.csv\"}}}}\n\
         ]}}\n\
         ```\n\n\
         示例 2 (多节点 + 结果传递):\n\
         ```json\n\
         {{\"template_kind\": \"normal\", \"nodes\": [\n\
           {{\"id\": \"n1\", \"depends_on\": [], \"task_description\": \"列出 ./data\", \"expected_output\": \"文件列表\",\n\
             \"capability\": \"file.list\", \"prefilled_arguments\": {{\"path\": \"./data\"}}}},\n\
           {{\"id\": \"n2\", \"depends_on\": [\"n1\"], \"task_description\": \"统计 sales.csv 类别总额\", \"expected_output\": \"top5\",\n\
             \"capability\": \"shell.exec\"}}\n\
         ]}}\n\
         ```\n\n\
         示例 3 (定时任务):\n\
         ```json\n\
         {{\"template_kind\": \"scheduled\", \"trigger\": {{\"type\": \"cron\", \"schedule\": \"0 9 * * *\"}}, \"nodes\": [\n\
           {{\"id\": \"n1\", \"depends_on\": [], \"task_description\": \"抓取比特币行情\", \"capability\": \"shell.exec\"}}\n\
         ]}}\n\
         ```\n\n\
         规则:\n\
         - 一个节点只做一件事 (一种工具); 依赖节点输出会自动注入后续节点上下文\n\
         - **capability 必填**: 设计时为本节点配能力 (使用上表名称, 别名如 shell_exec/file_read 也会被归一)\n\
         - 若你能确定唯一正确的参数, 直接给 prefilled_arguments (按能力 schema);\n\
           否则省略, subagent 会为该能力生成参数并执行\n\
         - **读取文件前先确认存在**: 用 file.list 或 ls 检查 (工作区清单可参考);\n\
           目标文件不存在时必须设计创建它的节点, 不能假设产物已存在\n\
         - **依赖必须显式声明**: 节点需要使用前序节点产物时, 必须写 depends_on;\n\
           否则节点会并行执行, 前序产物尚不存在 → 失败 (反例: n3 读 n2 创建的脚本但不声明 depends_on → 并行执行 → 文件不存在)\n\
         - 不要写散文到 prefilled_arguments 中, 参数必须是可执行的值\n\
         - trigger 仅用于定时 (cron) / webhook / 内部事件, 普通任务省略",
        base, goal, constraints_str, caps_str, template_kind
    )
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    pool: Arc<AgentPool>,
    rx: mpsc::Receiver<AgentMessage>,
    provider: Arc<dyn LlmProvider>,
    model_row: ModelRow,
    api_key: SecretString,
    subagent_pool: Arc<SubAgentPool>,
    trivium_db: Option<Arc<tokio::sync::Mutex<TriviumDb>>>,
    product_store: Option<Arc<PlatformProductStore>>,
    cursor_store: Option<Arc<CursorStore>>,
    prompts_dir: Option<PathBuf>,
    capability_ids: Vec<String>,
    registry: Option<Registry>,
    executor: Option<Arc<CapabilityExecutor>>,
) {
    let platform = ExecutionPlatform::new(
        rx,
        pool,
        provider,
        model_row,
        api_key,
        subagent_pool,
        trivium_db,
        product_store,
        cursor_store,
        prompts_dir,
        capability_ids,
        registry,
        executor,
    );
    let handle = platform.spawn();

    match handle.await {
        Ok(()) => tracing::info!("execution_platform::run: platform spawn completed"),
        Err(e) => tracing::error!(
            "execution_platform::run: platform task panicked/aborted: {e} (thread death = channel closed)"
        ),
    }
}

fn thought_id_from_turn(turn_id: &str) -> ThoughtId {
    ThoughtId::parse(turn_id).unwrap_or_else(|_| {
        let uuid = Uuid::new_v5(&Uuid::NAMESPACE_OID, turn_id.as_bytes());
        ThoughtId::parse(&uuid.to_string()).expect("uuid v5 always parses")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_max_turns_reads_config_with_default_fallback() {
        use crate::data::duckdb::loader::Registry;

        let mut reg = Registry::new();
        assert_eq!(agent_max_turns_from_registry(&reg), None, "空注册表 → None");

        reg.agents.insert(
            "agent".to_string(),
            crate::data::duckdb::loader::AgentRow {
                id: "agent".to_string(),
                name: "Agent".to_string(),
                mode: "unni".to_string(),
                prompt: None,
                tool_caps: vec![],
                config: None,
                display_name: None,
                is_default: true,
            },
        );
        assert_eq!(
            agent_max_turns_from_registry(&reg),
            None,
            "无 config → None"
        );

        reg.agents.get_mut("agent").unwrap().config = Some(serde_json::json!({"max_turns": 8}));
        assert_eq!(
            agent_max_turns_from_registry(&reg),
            Some(8),
            "config.max_turns → 8"
        );

        reg.agents.get_mut("agent").unwrap().config =
            Some(serde_json::json!({"max_turns": "not-a-number"}));
        assert_eq!(agent_max_turns_from_registry(&reg), None, "非数字 → None");
    }

    #[test]
    fn agent_max_turns_prefers_default_agent_over_id_lookup() {
        use crate::data::duckdb::loader::Registry;

        let mut reg = Registry::new();
        reg.agents.insert(
            "other".to_string(),
            crate::data::duckdb::loader::AgentRow {
                id: "other".to_string(),
                name: "Other".to_string(),
                mode: "keep".to_string(),
                prompt: None,
                tool_caps: vec![],
                config: Some(serde_json::json!({"max_turns": 12})),
                display_name: None,
                is_default: true,
            },
        );
        reg.agents.insert(
            "agent".to_string(),
            crate::data::duckdb::loader::AgentRow {
                id: "agent".to_string(),
                name: "Agent".to_string(),
                mode: "unni".to_string(),
                prompt: None,
                tool_caps: vec![],
                config: Some(serde_json::json!({"max_turns": 6})),
                display_name: None,
                is_default: false,
            },
        );
        assert_eq!(
            agent_max_turns_from_registry(&reg),
            Some(12),
            "应优先 is_default agent 的配置"
        );
    }

    #[test]
    fn prefilled_small_arguments_stay_prefilled() {
        let args = serde_json::json!({"path": "Cargo.toml"});
        assert_eq!(prefilled_arguments_oversized(&args), None);
    }

    #[test]
    fn prefilled_oversized_content_degrades_to_subagent() {
        let big = "x".repeat(10_000);
        let args = serde_json::json!({"content": big});
        let bytes = prefilled_arguments_oversized(&args).expect("应为超限");
        assert!(bytes > PREFILLED_MAX_BYTES, "got {bytes} bytes");
    }

    #[test]
    fn prefilled_at_threshold_boundary() {
        let args = serde_json::json!({"path": "a".repeat(PREFILLED_MAX_BYTES)});
        let bytes = prefilled_arguments_oversized(&args).expect("带 JSON 包装后应超限");
        assert!(bytes > PREFILLED_MAX_BYTES, "got {bytes} bytes");
    }

    #[test]
    fn design_retry_prompt_keeps_error_and_adds_guidance() {
        let prompt = design_retry_prompt("BASE", "line 1: expected value at line 3 column 7");
        assert!(prompt.starts_with("BASE"), "原 prompt 前置保留");
        assert!(
            prompt.contains("line 1: expected value at line 3 column 7"),
            "解析错误信息(含行列位置)原样保留"
        );
        assert!(
            prompt.contains("不要把它放进 prefilled_arguments"),
            "缺少过长内容降级指引"
        );
        assert!(
            prompt.contains("prefilled_arguments 必须严格匹配能力 schema 的 required 字段与类型"),
            "缺少 schema 匹配指引"
        );
    }

    #[test]
    fn parse_execution_design_prefers_task_flow() {
        let content = r#"{
            "template_kind": "normal",
            "nodes": [
                {"id": "n1", "depends_on": [], "task_description": "read file",
                 "expected_output": "content",
                 "capability": "file.read", "prefilled_arguments": {"path": "Cargo.toml"}},
                {"id": "n2", "depends_on": ["n1"], "task_description": "count lines",
                 "expected_output": "count", "capability": "text.grep"}
            ]
        }"#;
        match parse_execution_design(content) {
            Ok(ExecutionDesign::Flow(flow)) => {
                assert_eq!(flow.nodes.len(), 2);
                let n1 = &flow.nodes[0];
                assert_eq!(n1.id, "n1");
                assert!(n1.depends_on.is_empty());
                assert_eq!(n1.capability, "file.read");
                let args = n1.prefilled_arguments.as_ref().unwrap();
                assert_eq!(args["path"], "Cargo.toml");
                let n2 = &flow.nodes[1];
                assert_eq!(n2.depends_on, vec!["n1".to_string()]);
                assert_eq!(n2.capability, "text.grep");
                assert!(n2.prefilled_arguments.is_none(), "两层式: 无预填");
            }
            other => panic!("expected Flow, got: {other:?}"),
        }
    }

    #[test]
    fn parse_execution_design_flow_from_json_block() {
        let content = "```json\n{\"template_kind\": \"normal\", \"nodes\": [{\"id\": \"a\", \"depends_on\": [], \"task_description\": \"x\", \"expected_output\": \"y\", \"capability\": \"file.read\"}]}\n```";
        assert!(matches!(
            parse_execution_design(content),
            Ok(ExecutionDesign::Flow(_))
        ));
    }

    #[test]
    fn tolerant_parse_recovers_from_trailing_comma_and_truncation() {
        let content = r#"{"template_kind": "normal", "nodes": [
            {"id": "n1", "depends_on": [], "task_description": "a", "capability": "file.read"},
            {"id": "n2", "depends_on": ["n1"], "task_description": "b", "capability": "text.grep"},"#;
        match parse_execution_design(content) {
            Ok(ExecutionDesign::Flow(flow)) => {
                assert_eq!(flow.nodes.len(), 2);
                assert_eq!(flow.nodes[0].id, "n1");
            }
            other => panic!("expected Flow (truncation repaired), got: {other:?}"),
        }
    }

    #[test]
    fn tolerant_parse_marks_failed_nodes_and_recovers_others() {
        let content = r#"{"template_kind": "normal", "nodes": [
            {"id": "n1", "depends_on": [], "task_description": "a"},
            {"id": "n2", "depends_on": ["n1"], "task_description": "b", "capability": "file.read"}
        ]}"#;
        match parse_execution_design(content) {
            Ok(ExecutionDesign::Flow(flow)) => {
                assert_eq!(flow.nodes.len(), 1, "失败节点应跳过, 成功节点保留");
                assert_eq!(flow.nodes[0].id, "n2");
            }
            other => panic!("expected Flow (partial recovery), got: {other:?}"),
        }
    }

    #[test]
    fn tolerant_parse_never_falls_back_to_single_for_flow_shape() {
        let content = r#"{"template_kind": "normal", "nodes": [
            {"id": "n1", "depends_on": []},
            {"id": "n2", "depends_on": []}
        ]}"#;
        let result = parse_execution_design(content);
        assert!(result.is_err(), "Flow 形状全失败必须显式错误 (不落 Single)");
        assert!(
            !matches!(result, Ok(ExecutionDesign::Single(_))),
            "绝不能落 Single 兜底"
        );
    }

    #[test]
    fn tolerant_parse_handles_extra_closing_brace() {
        let content = r#"{"template_kind": "normal", "nodes": [
            {"id": "n1", "depends_on": [], "task_description": "a", "capability": "file.read", "prefilled_arguments": {"path": "x.txt"}}},
            {"id": "n2", "depends_on": ["n1"], "task_description": "b", "capability": "text.grep"}
        ]}"#;
        match parse_execution_design(content) {
            Ok(ExecutionDesign::Flow(flow)) => {
                assert_eq!(flow.nodes.len(), 2, "多余闭合括号应被修复");
                assert_eq!(flow.nodes[0].id, "n1");
                assert_eq!(flow.nodes[1].id, "n2");
            }
            other => panic!("expected Flow (extra brace repaired), got: {other:?}"),
        }
    }

    #[test]
    fn tolerant_parse_extracts_nodes_when_whole_json_invalid() {
        let content = r#"{"template_kind":"normal","nodes":[{"id":"n1","depends_on":[],"task_description":"a","capability":"file.read"},{"id":"n2","depends_on":["n1"],"task_description":"b","capability":"text.grep"}],"extra":unclosed"#;
        match parse_execution_design(content) {
            Ok(ExecutionDesign::Flow(flow)) => {
                assert_eq!(flow.nodes.len(), 2, "逐节点提取应恢复节点");
                assert_eq!(flow.nodes[0].capability, "file.read");
                assert_eq!(flow.nodes[1].depends_on, vec!["n1".to_string()]);
            }
            other => panic!("expected Flow (node extraction), got: {other:?}"),
        }
    }

    #[test]
    fn tolerant_parse_never_returns_empty_extraction() {
        let content = "not json at all with no nodes";
        assert!(extract_flow_nodes_tolerant(content).is_none());
    }

    #[test]
    fn trigger_spec_parses_cron_and_event() {
        let cron = r#"{"template_kind": "scheduled", "trigger": {"type": "cron", "schedule": "60"}, "nodes": [{"id": "n1", "depends_on": [], "task_description": "x", "capability": "file.read"}]}"#;
        match parse_execution_design(cron) {
            Ok(ExecutionDesign::Flow(flow)) => {
                assert!(matches!(flow.trigger, Some(TriggerSpec::Cron { .. })));
                if let Some(TriggerSpec::Cron { schedule }) = &flow.trigger {
                    assert_eq!(schedule, "60");
                }
            }
            other => panic!("expected Flow with cron trigger, got: {other:?}"),
        }
        let event = r#"{"template_kind": "triggered", "trigger": {"type": "event", "kind": "memory_complete"}, "nodes": [{"id": "n1", "depends_on": [], "task_description": "x", "capability": "file.read"}]}"#;
        match parse_execution_design(event) {
            Ok(ExecutionDesign::Flow(flow)) => {
                assert!(matches!(flow.trigger, Some(TriggerSpec::Event { .. })));
            }
            other => panic!("expected Flow with event trigger, got: {other:?}"),
        }
    }

    #[test]
    fn parse_schedule_to_seconds_variants() {
        assert_eq!(parse_schedule_to_seconds("60"), Some(60));
        assert_eq!(parse_schedule_to_seconds("30s"), Some(30));
        assert_eq!(parse_schedule_to_seconds("5m"), Some(300));
        assert_eq!(parse_schedule_to_seconds("*/5"), Some(300));
        assert_eq!(
            parse_schedule_to_seconds("0 9 * * *"),
            None,
            "完整 cron 未支持"
        );
        assert_eq!(parse_schedule_to_seconds("abc"), None);
    }

    #[test]
    fn webhook_listen_addr_parses_urls() {
        assert_eq!(
            webhook_listen_addr("http://localhost:9090/cipher-trigger"),
            Some("localhost:9090".to_string())
        );
        assert_eq!(
            webhook_listen_addr("localhost:8080"),
            Some("localhost:8080".to_string())
        );
        assert_eq!(
            webhook_listen_addr("http://0.0.0.0:1234/"),
            Some("0.0.0.0:1234".to_string())
        );
        assert_eq!(webhook_listen_addr(""), None);
    }

    #[tokio::test]
    async fn event_subscription_fires_task_flow_on_kind_match() {
        use crate::agent::agent_pool::AgentPool;
        let (pool, _receivers) = AgentPool::new();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("evt.txt"), "event-triggered").unwrap();
        let runner = NodeRunner {
            registry: Some(a1_flow_registry()),
            executor: Some(Arc::new(a1_flow_executor(tmp.path()))),
            provider: Arc::new(SequenceProvider::new(vec![])),
            model_row: ModelRow {
                id: "m".into(),
                name: "m".into(),
                provider: "p".into(),
                api_url: "http://localhost".into(),
                api_type: "OpenAI".into(),
                api_protocol: "openai-v1".into(),
                api_key: None,
                model_id: "m".into(),
                config: None,
            },
            api_key: SecretString::new("sk-x".into()),
        };
        let flow = TaskFlow {
            template_kind: "triggered".to_string(),
            trigger: Some(TriggerSpec::Event {
                kind: "memory_complete".to_string(),
            }),
            nodes: vec![flow_node(
                "n1",
                vec![],
                "read event file",
                "file.read",
                Some(serde_json::json!({"path": "evt.txt"})),
            )],
        };

        let mut rx = pool.subscribe_events();
        pool.publish_event("memory_complete", "t1");
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("event should arrive")
            .expect("channel open");
        assert_eq!(event.kind, "memory_complete");
        assert_eq!(event.detail, "t1");

        let results = runner.execute_flow_public(&flow).await;
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].status,
            NodeStatus::Completed,
            "event-triggered TaskFlow must complete: {:?}",
            results[0].error
        );
        assert!(
            results[0].summary.contains("event-triggered"),
            "summary: {}",
            results[0].summary
        );
    }

    #[tokio::test]
    async fn webhook_listener_serves_request_and_fires() {
        use crate::agent::agent_pool::AgentPool;
        use std::io::{Read, Write};
        let (_pool, _receivers) = AgentPool::new();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("hook.txt"), "hook-triggered").unwrap();
        let runner = NodeRunner {
            registry: Some(a1_flow_registry()),
            executor: Some(Arc::new(a1_flow_executor(tmp.path()))),
            provider: Arc::new(SequenceProvider::new(vec![])),
            model_row: ModelRow {
                id: "m".into(),
                name: "m".into(),
                provider: "p".into(),
                api_url: "http://localhost".into(),
                api_type: "OpenAI".into(),
                api_protocol: "openai-v1".into(),
                api_key: None,
                model_id: "m".into(),
                config: None,
            },
            api_key: SecretString::new("sk-x".into()),
        };
        let flow = TaskFlow {
            template_kind: "triggered".to_string(),
            trigger: Some(TriggerSpec::Webhook {
                url: "http://127.0.0.1:18091/hook".to_string(),
            }),
            nodes: vec![flow_node(
                "n1",
                vec![],
                "read hook file",
                "file.read",
                Some(serde_json::json!({"path": "hook.txt"})),
            )],
        };

        let runner_hook = runner.clone();
        let flow_hook = flow.clone();
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
        std::thread::spawn(move || {
            let listener = std::net::TcpListener::bind("127.0.0.1:18091").unwrap();
            let _ = ready_tx.send(());
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK");
            let _ = stream.flush();
            drop(stream);

            let rt = tokio::runtime::Runtime::new().expect("runtime");
            let results = rt.block_on(runner_hook.execute_flow_public(&flow_hook));
            let _ = result_tx.send(results);
        });

        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), ready_rx)
            .await
            .expect("listener ready");
        let mut client = std::net::TcpStream::connect("127.0.0.1:18091").unwrap();
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        client
            .write_all(b"GET /hook HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let mut response = [0u8; 64];
        let n = client.read(&mut response).unwrap_or(0);
        let response = String::from_utf8_lossy(&response[..n]);
        assert!(
            response.contains("200 OK"),
            "webhook 应返回 200: {response}"
        );

        let results = tokio::time::timeout(std::time::Duration::from_secs(10), result_rx)
            .await
            .expect("webhook task should finish")
            .expect("no panic");
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].status,
            NodeStatus::Completed,
            "webhook-triggered TaskFlow must complete: {:?}",
            results[0].error
        );
        assert!(results[0].summary.contains("hook-triggered"));
    }

    #[test]
    fn parse_subagent_output_tool_call_and_done() {
        match parse_subagent_output(r#"{"arguments": {"command": "ls"}}"#) {
            SubagentAction::Arguments { arguments } => {
                assert_eq!(arguments["command"], "ls");
            }
            other => panic!("expected Arguments, got: {other:?}"),
        }
        match parse_subagent_output(
            r#"{"tool_call": {"name": "shell.exec", "arguments": {"command": "ls"}}}"#,
        ) {
            SubagentAction::Arguments { arguments } => {
                assert_eq!(arguments["command"], "ls");
            }
            other => panic!("expected Arguments (tool_call compat), got: {other:?}"),
        }
        match parse_subagent_output(r#"{"done": true, "summary": "done reading"}"#) {
            SubagentAction::Done { summary } => assert_eq!(summary, "done reading"),
            other => panic!("expected Done, got: {other:?}"),
        }
        assert!(matches!(
            parse_subagent_output("not json"),
            SubagentAction::Invalid(_)
        ));
    }

    #[test]
    fn parse_subagent_output_with_think_prefix() {
        let content = "<think>The task is to read the file first.</think>\n\
                       {\"done\": true, \"summary\": \"read done\"}";
        match parse_subagent_output(content) {
            SubagentAction::Done { summary } => assert_eq!(summary, "read done"),
            other => panic!("expected Done, got: {other:?}"),
        }
    }

    #[test]
    fn parse_subagent_output_with_think_prefix_arguments() {
        let content = "<think>need file path</think>{\"arguments\": {\"path\": \"top5.md\"}}";
        match parse_subagent_output(content) {
            SubagentAction::Arguments { arguments } => {
                assert_eq!(arguments["path"], "top5.md");
            }
            other => panic!("expected Arguments, got: {other:?}"),
        }
    }

    #[test]
    fn parse_subagent_output_unclosed_think_strips_to_end() {
        let content = "<think>still reasoning...\n{\"done\": true, \"summary\": \"ok\"}";
        assert!(matches!(
            parse_subagent_output(content),
            SubagentAction::Invalid(_)
        ));
    }

    #[test]
    fn parse_subagent_output_rejects_non_object_arguments() {
        for bad in [r#"{"arguments": "see result"}"#, r#"{"arguments": "text"}"#] {
            match parse_subagent_output(bad) {
                SubagentAction::Invalid(reason) => assert!(
                    reason.contains("arguments 必须是 JSON 对象"),
                    "reason must explain object requirement: {reason}"
                ),
                other => panic!("expected Invalid for {bad}, got: {other:?}"),
            }
        }
    }

    #[test]
    fn parse_subagent_output_rejects_non_object_tool_call_arguments() {
        for bad in [
            r#"{"tool_call": {"name": "file.read", "arguments": "text"}}"#,
            r#"{"tool_call": {"name": "file.read", "arguments": null}}"#,
        ] {
            match parse_subagent_output(bad) {
                SubagentAction::Invalid(reason) => assert!(
                    reason.contains("arguments 必须是 JSON 对象"),
                    "reason must explain object requirement: {reason}"
                ),
                other => panic!("expected Invalid for {bad}, got: {other:?}"),
            }
        }
    }

    #[test]
    fn subagent_output_invalid_with_cjk_truncation_is_safe() {
        let content = "本".repeat(100);
        let result = parse_subagent_output(&content);
        assert!(matches!(result, SubagentAction::Invalid(_)));
    }

    #[test]
    fn resolve_capability_alias_normalizes_model_names() {
        assert_eq!(resolve_capability_alias("shell_exec"), "shell.exec");
        assert_eq!(resolve_capability_alias("functions.file_read"), "file.read");
        assert_eq!(resolve_capability_alias("file.read"), "file.read");
        assert_eq!(resolve_capability_alias("db_query"), "db.query");
        assert_eq!(resolve_capability_alias("unknown.tool"), "unknown.tool");
    }

    #[test]
    fn topological_sort_works_for_task_nodes() {
        let nodes = vec![
            TaskNode {
                id: "n2".into(),
                depends_on: vec!["n1".into()],
                task_description: "b".into(),
                expected_output: String::new(),
                capability: "file.read".into(),
                prefilled_arguments: None,
            },
            TaskNode {
                id: "n1".into(),
                depends_on: vec![],
                task_description: "a".into(),
                expected_output: String::new(),
                capability: "file.read".into(),
                prefilled_arguments: None,
            },
        ];
        let order = topological_sort(&nodes).unwrap();
        assert_eq!(order, vec!["n1".to_string(), "n2".to_string()]);
    }

    #[test]
    fn task_flow_cycle_detected() {
        let nodes = vec![
            TaskNode {
                id: "a".into(),
                depends_on: vec!["b".into()],
                task_description: String::new(),
                expected_output: String::new(),
                capability: "file.read".into(),
                prefilled_arguments: None,
            },
            TaskNode {
                id: "b".into(),
                depends_on: vec!["a".into()],
                task_description: String::new(),
                expected_output: String::new(),
                capability: "file.read".into(),
                prefilled_arguments: None,
            },
        ];
        assert!(topological_sort(&nodes).is_err());
    }

    #[test]
    fn parse_task_context_json_object_passthrough() {
        let v = parse_task_context("file.write", r#"{"path":"/tmp/a.txt","content":"hi"}"#);
        assert_eq!(v["path"], "/tmp/a.txt");
        assert_eq!(v["content"], "hi");
    }

    #[test]
    fn parse_task_context_plain_text_defaults_by_capability() {
        assert_eq!(
            parse_task_context("file.read", "Cargo.toml"),
            serde_json::json!({"path": "Cargo.toml"})
        );
        assert_eq!(
            parse_task_context("shell.exec", "ls -la"),
            serde_json::json!({"command": "ls -la"})
        );
        assert_eq!(
            parse_task_context("code.exec", "print(1)"),
            serde_json::json!({"code": "print(1)"})
        );
        assert_eq!(
            parse_task_context("unknown.cap", "x"),
            serde_json::json!({"input": "x"})
        );
    }

    #[test]
    fn parse_task_context_non_object_json_falls_back_to_default() {
        let v = parse_task_context("file.read", "[1,2,3]");
        assert_eq!(v, serde_json::json!({"path": "[1,2,3]"}));
    }

    struct NullProvider;
    #[async_trait::async_trait]
    impl LlmProvider for NullProvider {
        fn id(&self) -> &'static str {
            "null"
        }
        fn name(&self) -> &'static str {
            "null"
        }
    }

    fn a1_registry() -> Registry {
        let mut reg = Registry::new();
        reg.base_capabilities.insert(
            "db.query".to_string(),
            crate::data::duckdb::loader::BaseCapabilityRow {
                id: "db.query".to_string(),
                name: "Query DB".to_string(),
                cap_type: "function".to_string(),
                description: "query table".to_string(),
                schema_in: serde_json::json!({"type":"object","properties":{"table":{"type":"string"}},"required":["table"]}),
                schema_out: serde_json::json!({}),
                executor: "builtin:db.query".to_string(),
                version: "1.0.0".to_string(),
                enabled: true,
                tombstoned_at: None,
                metadata: None,
            },
        );
        reg.agents.insert(
            "agent".to_string(),
            crate::data::duckdb::loader::AgentRow {
                id: "agent".to_string(),
                name: "Agent".to_string(),
                mode: "unni".to_string(),
                prompt: None,
                tool_caps: vec!["db.query".to_string()],
                config: None,
                display_name: None,
                is_default: true,
            },
        );
        reg
    }

    fn a1_executor() -> CapabilityExecutor {
        let conn = duckdb::Connection::open_in_memory().expect("in-memory duckdb");
        conn.execute_batch("CREATE TABLE agent (id TEXT); INSERT INTO agent VALUES ('agent');")
            .expect("seed agent table");
        let mut ex = CapabilityExecutor::new();
        ex.set_duckdb(std::sync::Arc::new(std::sync::Mutex::new(conn)));
        ex
    }

    fn a1_platform(
        registry: Option<Registry>,
        executor: Option<Arc<CapabilityExecutor>>,
    ) -> ExecutionPlatform {
        let (_tx, rx) = mpsc::channel(1);
        ExecutionPlatform::new(
            rx,
            Arc::new(AgentPool::new().0),
            Arc::new(NullProvider),
            ModelRow {
                id: "m".into(),
                name: "m".into(),
                provider: "p".into(),
                api_url: "http://localhost".into(),
                api_type: "OpenAI".into(),
                api_protocol: "openai-v1".into(),
                api_key: None,
                model_id: "m".into(),
                config: None,
            },
            SecretString::new("sk-x".into()),
            Arc::new(SubAgentPool::new()),
            None,
            None,
            None,
            None,
            vec![],
            registry,
            executor,
        )
    }

    fn a1_platform_with_provider(
        registry: Option<Registry>,
        executor: Option<Arc<CapabilityExecutor>>,
        provider: Arc<dyn LlmProvider>,
    ) -> ExecutionPlatform {
        let (_tx, rx) = mpsc::channel(1);
        ExecutionPlatform::new(
            rx,
            Arc::new(AgentPool::new().0),
            provider,
            ModelRow {
                id: "m".into(),
                name: "m".into(),
                provider: "p".into(),
                api_url: "http://localhost".into(),
                api_type: "OpenAI".into(),
                api_protocol: "openai-v1".into(),
                api_key: None,
                model_id: "m".into(),
                config: None,
            },
            SecretString::new("sk-x".into()),
            Arc::new(SubAgentPool::new()),
            None,
            None,
            None,
            None,
            vec![],
            registry,
            executor,
        )
    }

    use crate::logic::model::provider::LlmResponse;
    struct SequenceProvider {
        responses: std::sync::Mutex<std::collections::VecDeque<String>>,
    }
    impl SequenceProvider {
        fn new(responses: Vec<&str>) -> Self {
            Self {
                responses: std::sync::Mutex::new(
                    responses.into_iter().map(str::to_string).collect(),
                ),
            }
        }
    }
    #[async_trait::async_trait]
    impl LlmProvider for SequenceProvider {
        fn id(&self) -> &'static str {
            "sequence"
        }
        fn name(&self) -> &'static str {
            "sequence"
        }
        async fn call(&self, _req: &LlmRequest) -> Result<LlmResponse> {
            let content = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_default();
            Ok(LlmResponse {
                content,
                tool_calls: vec![],
                usage: None,
            })
        }
    }

    struct RecordingSequenceProvider {
        captured: std::sync::Mutex<Vec<Vec<ChatMessage>>>,
        responses: std::sync::Mutex<std::collections::VecDeque<String>>,
    }
    impl RecordingSequenceProvider {
        fn new(responses: Vec<&str>) -> Self {
            Self {
                captured: std::sync::Mutex::new(vec![]),
                responses: std::sync::Mutex::new(
                    responses.into_iter().map(str::to_string).collect(),
                ),
            }
        }
    }
    #[async_trait::async_trait]
    impl LlmProvider for RecordingSequenceProvider {
        fn id(&self) -> &'static str {
            "recording-sequence"
        }
        fn name(&self) -> &'static str {
            "recording-sequence"
        }
        async fn call(&self, req: &LlmRequest) -> Result<LlmResponse> {
            self.captured.lock().unwrap().push(req.messages.clone());
            let content = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_default();
            Ok(LlmResponse {
                content,
                tool_calls: vec![],
                usage: None,
            })
        }
    }

    fn a1_flow_registry() -> Registry {
        let mut reg = Registry::new();
        let file_read = crate::data::duckdb::loader::BaseCapabilityRow {
            id: "file.read".to_string(),
            name: "Read File".to_string(),
            cap_type: "function".to_string(),
            description: "read file".to_string(),
            schema_in: serde_json::json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
            schema_out: serde_json::json!({}),
            executor: "builtin:file.read".to_string(),
            version: "1.0.0".to_string(),
            enabled: true,
            tombstoned_at: None,
            metadata: None,
        };
        reg.base_capabilities
            .insert("file.read".to_string(), file_read);
        let file_write = crate::data::duckdb::loader::BaseCapabilityRow {
            id: "file.write".to_string(),
            name: "Write File".to_string(),
            cap_type: "function".to_string(),
            description: "write file".to_string(),
            schema_in: serde_json::json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}),
            schema_out: serde_json::json!({}),
            executor: "builtin:file.write".to_string(),
            version: "1.0.0".to_string(),
            enabled: true,
            tombstoned_at: None,
            metadata: None,
        };
        reg.base_capabilities
            .insert("file.write".to_string(), file_write);
        reg.base_capabilities.insert(
            "db.query".to_string(),
            crate::data::duckdb::loader::BaseCapabilityRow {
                id: "db.query".to_string(),
                name: "Query DB".to_string(),
                cap_type: "function".to_string(),
                description: "query table".to_string(),
                schema_in: serde_json::json!({"type":"object","properties":{"table":{"type":"string"}},"required":["table"]}),
                schema_out: serde_json::json!({}),
                executor: "builtin:db.query".to_string(),
                version: "1.0.0".to_string(),
                enabled: true,
                tombstoned_at: None,
                metadata: None,
            },
        );
        reg.agents.insert(
            "agent".to_string(),
            crate::data::duckdb::loader::AgentRow {
                id: "agent".to_string(),
                name: "agent".to_string(),
                mode: "unni".to_string(),
                prompt: None,
                tool_caps: vec![
                    "file.read".to_string(),
                    "file.write".to_string(),
                    "db.query".to_string(),
                ],
                config: None,
                display_name: Some("agent".to_string()),
                is_default: false,
            },
        );
        reg
    }

    fn a1_flow_executor(workspace: &Path) -> CapabilityExecutor {
        let mut ex = CapabilityExecutor::new();
        ex.set_workspace_root(workspace);
        let conn = duckdb::Connection::open_in_memory().expect("in-memory duckdb");
        conn.execute_batch("CREATE TABLE agent (id TEXT); INSERT INTO agent VALUES ('agent');")
            .expect("seed agent table");
        ex.set_duckdb(std::sync::Arc::new(std::sync::Mutex::new(conn)));
        ex
    }

    fn flow_node(
        id: &str,
        deps: Vec<&str>,
        desc: &str,
        capability: &str,
        prefilled_arguments: Option<serde_json::Value>,
    ) -> TaskNode {
        TaskNode {
            id: id.to_string(),
            depends_on: deps.into_iter().map(str::to_string).collect(),
            task_description: desc.to_string(),
            expected_output: String::new(),
            capability: capability.to_string(),
            prefilled_arguments,
        }
    }

    #[tokio::test]
    async fn a1_flow_prefilled_single_node_real_wasm_read() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("notes.txt"), "flow-test-content").unwrap();
        let platform = a1_platform(
            Some(a1_flow_registry()),
            Some(Arc::new(a1_flow_executor(tmp.path()))),
        );
        let flow = TaskFlow {
            template_kind: "normal".to_string(),
            trigger: None,
            nodes: vec![flow_node(
                "n1",
                vec![],
                "read notes",
                "file.read",
                Some(serde_json::json!({"path": "notes.txt"})),
            )],
        };
        let results = platform.execute_flow(&flow).await;
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].status,
            NodeStatus::Completed,
            "file.read must complete: {:?}",
            results[0].error
        );
        assert!(
            results[0].summary.contains("flow-test-content"),
            "summary must contain file content: {}",
            results[0].summary
        );
        assert_eq!(results[0].tool_call_count, 1);
    }

    #[tokio::test]
    async fn a1_flow_two_nodes_dependency_passes_result() {
        let tmp = tempfile::tempdir().unwrap();
        let platform = a1_platform(
            Some(a1_flow_registry()),
            Some(Arc::new(a1_flow_executor(tmp.path()))),
        );
        let flow = TaskFlow {
            template_kind: "normal".to_string(),
            trigger: None,
            nodes: vec![
                flow_node(
                    "n1",
                    vec![],
                    "write file",
                    "file.write",
                    Some(serde_json::json!({"path": "out.txt", "content": "hello-flow"})),
                ),
                flow_node(
                    "n2",
                    vec!["n1"],
                    "read the file n1 wrote",
                    "file.read",
                    Some(serde_json::json!({"path": "out.txt"})),
                ),
            ],
        };
        let results = platform.execute_flow(&flow).await;
        assert_eq!(results.len(), 2);
        for r in &results {
            assert_eq!(
                r.status,
                NodeStatus::Completed,
                "node {} must complete: {:?}",
                r.node_id,
                r.error
            );
        }
        assert!(
            results[1].summary.contains("hello-flow"),
            "n2 must read n1's output: {}",
            results[1].summary
        );
        assert!(std::fs::read_to_string(tmp.path().join("out.txt"))
            .unwrap()
            .contains("hello-flow"));
    }

    #[tokio::test]
    async fn a1_flow_prefilled_failure_isolated_other_nodes_continue() {
        let tmp = tempfile::tempdir().unwrap();
        let platform = a1_platform(
            Some(a1_flow_registry()),
            Some(Arc::new(a1_flow_executor(tmp.path()))),
        );
        let flow = TaskFlow {
            template_kind: "normal".to_string(),
            trigger: None,
            nodes: vec![
                flow_node(
                    "n1",
                    vec![],
                    "read missing",
                    "file.read",
                    Some(serde_json::json!({"path": "does_not_exist.txt"})),
                ),
                flow_node(
                    "n2",
                    vec![],
                    "query db",
                    "db.query",
                    Some(serde_json::json!({"table": "agent"})),
                ),
            ],
        };
        let results = platform.execute_flow(&flow).await;
        assert_eq!(results.len(), 2);
        let n1 = results
            .iter()
            .find(|r| r.node_id == "n1")
            .expect("n1 result present");
        let n2 = results
            .iter()
            .find(|r| r.node_id == "n2")
            .expect("n2 result present");
        assert_eq!(n1.status, NodeStatus::Failed);
        assert!(n1.error.is_some(), "failed node must carry error reason");
        assert_eq!(
            n2.status,
            NodeStatus::Completed,
            "independent node must still execute: {:?}",
            n2.error
        );
    }

    #[tokio::test]
    async fn a1_flow_repeated_execution_is_stable() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("stable.txt"), "stable-value").unwrap();
        let platform = a1_platform(
            Some(a1_flow_registry()),
            Some(Arc::new(a1_flow_executor(tmp.path()))),
        );
        let flow = TaskFlow {
            template_kind: "normal".to_string(),
            trigger: None,
            nodes: vec![flow_node(
                "n1",
                vec![],
                "read stable",
                "file.read",
                Some(serde_json::json!({"path": "stable.txt"})),
            )],
        };
        for i in 0..5 {
            let results = platform.execute_flow(&flow).await;
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].status, NodeStatus::Completed, "iteration {i}");
            assert!(
                results[0].summary.contains("stable-value"),
                "iteration {i} summary: {}",
                results[0].summary
            );
        }
    }

    #[tokio::test]
    async fn a1_subagent_loop_two_stage_with_mock_provider() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("loop.txt"), "loop-result").unwrap();
        let provider = Arc::new(SequenceProvider::new(vec![
            r#"{"tool_call": {"name": "file.read", "arguments": {"path": "loop.txt"}}}"#,
            r#"{"done": true, "summary": "read loop.txt successfully"}"#,
        ]));
        let platform = a1_platform_with_provider(
            Some(a1_flow_registry()),
            Some(Arc::new(a1_flow_executor(tmp.path()))),
            provider,
        );
        let node = flow_node("n1", vec![], "read loop.txt", "file.read", None);
        let (result, _turns) = platform.node_runner().run_subagent_loop(&node, "").await;
        assert_eq!(
            result.status,
            NodeStatus::Completed,
            "subagent loop must complete: {:?}",
            result.error
        );
        assert!(result.summary.contains("read loop.txt"));
        assert_eq!(result.tool_call_count, 1);
    }

    #[tokio::test]
    async fn subagent_loop_history_carries_real_arguments_not_see_result() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("loop.txt"), "loop-result").unwrap();
        let provider = Arc::new(RecordingSequenceProvider::new(vec![
            r#"{"tool_call": {"name": "file.read", "arguments": {"path": "loop.txt"}}}"#,
            r#"{"done": true, "summary": "read done"}"#,
        ]));
        let platform = a1_platform_with_provider(
            Some(a1_flow_registry()),
            Some(Arc::new(a1_flow_executor(tmp.path()))),
            provider.clone(),
        );
        let node = flow_node("n1", vec![], "read loop.txt", "file.read", None);
        let (result, _turns) = platform.node_runner().run_subagent_loop(&node, "").await;
        assert_eq!(
            result.status,
            NodeStatus::Completed,
            "subagent loop must complete: {:?}",
            result.error
        );
        let calls = provider.captured.lock().unwrap();
        assert!(
            calls.len() >= 2,
            "expect >=2 LLM calls, got {}",
            calls.len()
        );
        let second_call = &calls[1];
        let assistant_msgs: Vec<&ChatMessage> = second_call
            .iter()
            .filter(|m| matches!(m, ChatMessage::Assistant { .. }))
            .collect();
        assert_eq!(
            assistant_msgs.len(),
            1,
            "expect exactly one assistant history message"
        );
        let (assistant_text, assistant_tool_calls) = match &assistant_msgs[0] {
            ChatMessage::Assistant { text, tool_calls } => (text.as_str(), tool_calls.as_slice()),
            _ => unreachable!(),
        };
        assert!(
            assistant_tool_calls.is_empty(),
            "subagent 循环历史不使用 tool_calls"
        );
        assert!(
            assistant_text.contains("\"path\":\"loop.txt\""),
            "assistant history must carry real arguments: {assistant_text}"
        );
        assert!(
            !assistant_text.contains("see result"),
            "history must not contain 'see result': {assistant_text}"
        );
    }

    #[tokio::test]
    async fn a1_subagent_loop_invalid_output_recovers_then_done() {
        let tmp = tempfile::tempdir().unwrap();
        let provider = Arc::new(SequenceProvider::new(vec![
            "not json at all",
            r#"{"done": true, "summary": "recovered"}"#,
        ]));
        let platform = a1_platform_with_provider(
            Some(a1_flow_registry()),
            Some(Arc::new(a1_flow_executor(tmp.path()))),
            provider,
        );
        let node = flow_node("n1", vec![], "do something", "file.read", None);
        let (result, _turns) = platform.node_runner().run_subagent_loop(&node, "").await;
        assert_eq!(result.status, NodeStatus::Completed);
        assert_eq!(result.summary, "recovered");
    }

    #[tokio::test]
    async fn a1_subagent_loop_exhausts_turns_fails_gracefully() {
        let tmp = tempfile::tempdir().unwrap();
        let provider = Arc::new(SequenceProvider::new(vec![
            r#"{"tool_call": {"name": "file.read", "arguments": {"path": "missing_forever.txt"}}}"#,
            r#"{"tool_call": {"name": "file.read", "arguments": {"path": "missing_forever.txt"}}}"#,
            r#"{"tool_call": {"name": "file.read", "arguments": {"path": "missing_forever.txt"}}}"#,
            r#"{"tool_call": {"name": "file.read", "arguments": {"path": "missing_forever.txt"}}}"#,
            r#"{"tool_call": {"name": "file.read", "arguments": {"path": "missing_forever.txt"}}}"#,
            r#"{"tool_call": {"name": "file.read", "arguments": {"path": "missing_forever.txt"}}}"#,
            r#"{"tool_call": {"name": "file.read", "arguments": {"path": "missing_forever.txt"}}}"#,
        ]));
        let platform = a1_platform_with_provider(
            Some(a1_flow_registry()),
            Some(Arc::new(a1_flow_executor(tmp.path()))),
            provider,
        );
        let node = flow_node("n1", vec![], "do something", "file.read", None);
        let (result, _turns) = platform.node_runner().run_subagent_loop(&node, "").await;
        assert_eq!(result.status, NodeStatus::Failed);
        assert!(
            result.error.as_deref().unwrap_or("").contains("max_turns"),
            "error must explain turn exhaustion: {:?}",
            result.error
        );
    }

    #[test]
    fn build_dep_summary_injects_dependency_result_with_capability() {
        let nodes = [
            TaskNode {
                id: "n1".into(),
                depends_on: vec![],
                task_description: "probe".into(),
                expected_output: String::new(),
                capability: "shell.exec".into(),
                prefilled_arguments: None,
            },
            TaskNode {
                id: "n2".into(),
                depends_on: vec!["n1".into()],
                task_description: "write result".into(),
                expected_output: String::new(),
                capability: "file.write".into(),
                prefilled_arguments: None,
            },
        ];
        let mut results: HashMap<String, NodeResult> = HashMap::new();
        results.insert(
            "n1".to_string(),
            NodeResult {
                node_id: "n1".to_string(),
                status: NodeStatus::Completed,
                summary: "probe found: data/sales.csv".to_string(),
                error: None,
                tool_call_count: 1,
                tool_call_logs: vec![],
            },
        );
        let by_id: HashMap<String, &TaskNode> = nodes.iter().map(|n| (n.id.clone(), n)).collect();
        let s = build_dep_summary(&nodes[1], &results, &by_id);
        assert_eq!(
            s, "[n1] shell.exec 完成: probe found: data/sales.csv",
            "dep 行必须带节点 id + 能力 + 真实结果"
        );

        results.get_mut("n1").unwrap().summary = String::new();
        let s2 = build_dep_summary(&nodes[1], &results, &by_id);
        assert_eq!(s2, "[n1] shell.exec 完成 (无输出摘要)");
    }

    #[test]
    fn sanitize_args_summary_redacts_and_truncates() {
        let long_content = "x".repeat(200);
        let args = serde_json::json!({
            "path": "/tmp/out.txt",
            "content": long_content,
            "api_key": "sk-secret-123"
        });
        let s = sanitize_args_summary(&args);

        assert!(
            s.contains("args_keys=[api_key, content, path]"),
            "keys: {s}"
        );
        assert!(s.contains("api_key=<redacted>"), "redact: {s}");
        assert!(s.contains("content_len=200"), "len: {s}");
        assert!(s.contains("content_prefix=\"xxx"), "prefix: {s}");
        assert!(s.contains("path=\"/tmp/out.txt\""), "path 完整打印: {s}");
        assert!(!s.contains("sk-secret-123"), "secret 绝不能泄漏: {s}");
        assert!(s.len() < long_content.len(), "绝不能嵌入完整 content: {s}");

        let s2 = sanitize_args_summary(&serde_json::json!({"count": 42, "items": [1, 2, 3]}));
        assert!(s2.contains("count=42"), "non-string value: {s2}");
        let s3 = sanitize_args_summary(&serde_json::json!("just a string"));
        assert!(s3.contains("non-object"), "non-object: {s3}");
    }

    struct CapturingProvider {
        captured: std::sync::Mutex<Vec<String>>,
    }
    #[async_trait::async_trait]
    impl LlmProvider for CapturingProvider {
        fn id(&self) -> &'static str {
            "capture"
        }
        fn name(&self) -> &'static str {
            "capture"
        }
        async fn call(&self, req: &LlmRequest) -> Result<LlmResponse> {
            self.captured.lock().unwrap().push(
                req.messages
                    .first()
                    .map(|m| m.text().to_string())
                    .unwrap_or_default(),
            );
            Ok(LlmResponse {
                content: r#"{"done": true, "summary": "used dependency result"}"#.to_string(),
                tool_calls: vec![],
                usage: None,
            })
        }
    }

    #[tokio::test]
    async fn a1_flow_subagent_node_receives_dependency_result_summary() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("dep.txt"), "dependency-output-42").unwrap();
        let provider = Arc::new(CapturingProvider {
            captured: std::sync::Mutex::new(vec![]),
        });
        let platform = a1_platform_with_provider(
            Some(a1_flow_registry()),
            Some(Arc::new(a1_flow_executor(tmp.path()))),
            provider.clone(),
        );
        let flow = TaskFlow {
            template_kind: "normal".to_string(),
            trigger: None,
            nodes: vec![
                flow_node(
                    "n1",
                    vec![],
                    "read dep.txt",
                    "file.read",
                    Some(serde_json::json!({"path": "dep.txt"})),
                ),
                flow_node("n2", vec!["n1"], "use n1 result", "file.read", None),
            ],
        };
        let results = platform.execute_flow(&flow).await;
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].status, NodeStatus::Completed);
        assert_eq!(
            results[1].status,
            NodeStatus::Completed,
            "subagent n2 must complete: {:?}",
            results[1].error
        );
        let captured = provider.captured.lock().unwrap();
        assert_eq!(captured.len(), 1, "n2 两层式应调用 1 次 LLM: {captured:?}");
        let system = &captured[0];
        assert!(system.contains("依赖节点结果:"), "system: {system}");
        assert!(system.contains("[n1]"), "dep 行必须带节点 id: {system}");
        assert!(
            system.contains("file.read 完成"),
            "dep 行必须带能力: {system}"
        );
        assert!(
            system.contains("dependency-output-42"),
            "dep 行必须带前置真实结果: {system}"
        );
    }

    #[tokio::test]
    async fn a1_dispatch_single_executes_real_capability_via_service() {
        let platform = a1_platform(Some(a1_registry()), Some(Arc::new(a1_executor())));
        let design = SubAgentDesign {
            template_kind: "normal".to_string(),
            capability_ids: vec!["db.query".to_string()],
            task_context: r#"{"table":"agent"}"#.to_string(),
            arguments: None,
            max_turns: 1,
            timeout_seconds: 30,
        };
        let result = platform.dispatch_single_subagent(&design).await;
        assert_eq!(
            result.status,
            NodeStatus::Completed,
            "db.query 应真实执行成功: {:?}",
            result.error
        );
        assert_eq!(result.tool_call_count, 1, "应有 1 次能力调用");
        assert!(
            result
                .tool_call_logs
                .iter()
                .any(|l| l.contains("OK db.query")),
            "日志应含 OK db.query: {:?}",
            result.tool_call_logs
        );
    }

    #[tokio::test]
    async fn a1_dispatch_single_marks_failed_on_unauthorized_capability() {
        let platform = a1_platform(Some(a1_registry()), Some(Arc::new(a1_executor())));
        let design = SubAgentDesign {
            template_kind: "normal".to_string(),
            capability_ids: vec!["file.read".to_string()],
            task_context: "Cargo.toml".to_string(),
            arguments: None,
            max_turns: 1,
            timeout_seconds: 30,
        };
        let result = platform.dispatch_single_subagent(&design).await;
        assert_eq!(result.status, NodeStatus::Failed);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn a1_execute_dag_two_nodes_dependency_order() {
        let platform = a1_platform(Some(a1_registry()), Some(Arc::new(a1_executor())));
        let dag = DagDesign {
            template_kind: "dag".to_string(),
            nodes: vec![
                DagNodeDesign {
                    id: "n1".to_string(),
                    capability_ids: vec!["db.query".to_string()],
                    task_context: r#"{"table":"agent"}"#.to_string(),
                    arguments: None,
                    depends_on: vec![],
                    timeout_seconds: 30,
                },
                DagNodeDesign {
                    id: "n2".to_string(),
                    capability_ids: vec!["db.query".to_string()],
                    task_context: r#"{"table":"agent"}"#.to_string(),
                    arguments: None,
                    depends_on: vec!["n1".to_string()],
                    timeout_seconds: 30,
                },
            ],
        };
        let results = platform.execute_dag(&dag, "turn-test").await;
        assert_eq!(results.len(), 2);
        assert!(
            results.iter().all(|r| r.status == NodeStatus::Completed),
            "两节点都应完成: {:?}",
            results
                .iter()
                .map(|r| (&r.node_id, &r.status, &r.error))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn e1_design_arguments_take_precedence_over_task_context() {
        let platform = a1_platform(Some(a1_registry()), Some(Arc::new(a1_executor())));
        let design = SubAgentDesign {
            template_kind: "normal".to_string(),
            capability_ids: vec!["db.query".to_string()],

            task_context: "Query the agent table and count rows".to_string(),
            arguments: Some(serde_json::json!({"db.query": {"table": "agent"}})),
            max_turns: 1,
            timeout_seconds: 30,
        };
        let result = platform.dispatch_single_subagent(&design).await;
        assert_eq!(
            result.status,
            NodeStatus::Completed,
            "显式 arguments 应使执行成功: {:?}",
            result.error
        );
        assert_eq!(result.tool_call_count, 1);
    }

    #[test]
    fn e1_design_args_for_extracts_per_capability_object() {
        let args = serde_json::json!({
            "file.read": {"path": "a.txt"},
            "shell.exec": "not-an-object"
        });
        let got = ExecutionPlatform::design_args_for(Some(&args), "file.read");
        assert_eq!(got, Some(&serde_json::json!({"path": "a.txt"})));

        assert_eq!(
            ExecutionPlatform::design_args_for(Some(&args), "shell.exec"),
            None
        );
        assert_eq!(ExecutionPlatform::design_args_for(None, "file.read"), None);
    }

    #[test]
    fn e1_prompt_includes_capability_schema_when_registry_present() {
        let reg = a1_registry();
        let prompt = build_execution_prompt(
            "normal",
            "g",
            &[],
            None,
            &["db.query".to_string()],
            Some(&reg),
        );
        assert!(prompt.contains("db.query"), "prompt 应含能力 id: {prompt}");
        assert!(
            prompt.contains("参数 schema"),
            "prompt 应含 schema 标注: {prompt}"
        );
        assert!(prompt.contains("table"), "schema 内容应上 prompt: {prompt}");
    }

    #[tokio::test]
    async fn a1_no_runtime_degrades_to_simulation() {
        let platform = a1_platform(None, None);
        let design = SubAgentDesign {
            template_kind: "normal".to_string(),
            capability_ids: vec!["db.query".to_string()],
            task_context: "{}".to_string(),
            arguments: None,
            max_turns: 1,
            timeout_seconds: 30,
        };
        let result = platform.dispatch_single_subagent(&design).await;
        assert_eq!(
            result.status,
            NodeStatus::Completed,
            "无 runtime 应降级完成"
        );
        assert_eq!(result.tool_call_count, 0);
    }

    #[test]
    fn parse_single_design_json() {
        let json = r#"{"template_kind":"normal","capability_ids":["shell.exec"],"task_context":"do X","max_turns":5,"timeout_seconds":300}"#;
        let design = parse_execution_design(json).unwrap();
        match design {
            ExecutionDesign::Single(s) => {
                assert_eq!(s.template_kind, "normal");
                assert_eq!(s.capability_ids, vec!["shell.exec"]);
                assert_eq!(s.task_context, "do X");
                assert_eq!(s.max_turns, 5);
                assert_eq!(s.timeout_seconds, 300);
            }
            _ => panic!("expected Single"),
        }
    }

    #[test]
    fn parse_dag_design_json() {
        let json = r#"{"template_kind":"dag","nodes":[{"id":"n1","capability_ids":["shell.exec"],"task_context":"step 1","depends_on":[]},{"id":"n2","capability_ids":["http.request"],"task_context":"step 2","depends_on":["n1"]}]}"#;
        let design = parse_execution_design(json).unwrap();
        match design {
            ExecutionDesign::Dag(dag) => {
                assert_eq!(dag.template_kind, "dag");
                assert_eq!(dag.nodes.len(), 2);
                assert_eq!(dag.nodes[0].id, "n1");
                assert_eq!(dag.nodes[1].id, "n2");
                assert_eq!(dag.nodes[1].depends_on, vec!["n1"]);
            }
            _ => panic!("expected Dag"),
        }
    }

    #[test]
    fn parse_dag_in_code_block() {
        let text = "some text\n```json\n{\"template_kind\":\"dag\",\"nodes\":[{\"id\":\"n1\",\"capability_ids\":[\"shell.exec\"],\"task_context\":\"step 1\",\"depends_on\":[]}]}\n```\nmore text";
        let design = parse_execution_design(text).unwrap();
        match design {
            ExecutionDesign::Dag(dag) => {
                assert_eq!(dag.nodes.len(), 1);
                assert_eq!(dag.nodes[0].id, "n1");
            }
            _ => panic!("expected Dag"),
        }
    }

    #[test]
    fn parse_single_with_defaults() {
        let json = r#"{"template_kind":"normal","capability_ids":[],"task_context":"test"}"#;
        let design = parse_execution_design(json).unwrap();
        match design {
            ExecutionDesign::Single(s) => {
                assert_eq!(s.max_turns, 10);
                assert_eq!(s.timeout_seconds, 600);
            }
            _ => panic!("expected Single"),
        }
    }

    #[test]
    fn parse_invalid_returns_error() {
        let result = parse_execution_design("not json at all");
        assert!(result.is_err());
    }

    #[test]
    fn topo_sort_linear_dag() {
        let nodes = vec![
            DagNodeDesign {
                id: "n1".into(),
                capability_ids: vec![],
                task_context: "".into(),
                arguments: None,
                depends_on: vec![],
                timeout_seconds: 600,
            },
            DagNodeDesign {
                id: "n2".into(),
                capability_ids: vec![],
                task_context: "".into(),
                arguments: None,
                depends_on: vec!["n1".into()],
                timeout_seconds: 600,
            },
            DagNodeDesign {
                id: "n3".into(),
                capability_ids: vec![],
                task_context: "".into(),
                arguments: None,
                depends_on: vec!["n2".into()],
                timeout_seconds: 600,
            },
        ];
        let order = topological_sort(&nodes).unwrap();
        assert_eq!(order, vec!["n1", "n2", "n3"]);
    }

    #[test]
    fn topo_sort_fan_out() {
        let nodes = vec![
            DagNodeDesign {
                id: "root".into(),
                capability_ids: vec![],
                task_context: "".into(),
                arguments: None,
                depends_on: vec![],
                timeout_seconds: 600,
            },
            DagNodeDesign {
                id: "a".into(),
                capability_ids: vec![],
                task_context: "".into(),
                arguments: None,
                depends_on: vec!["root".into()],
                timeout_seconds: 600,
            },
            DagNodeDesign {
                id: "b".into(),
                capability_ids: vec![],
                task_context: "".into(),
                arguments: None,
                depends_on: vec!["root".into()],
                timeout_seconds: 600,
            },
        ];
        let order = topological_sort(&nodes).unwrap();
        assert_eq!(order[0], "root");

        let after_root: Vec<&str> = order[1..].iter().map(|s| s.as_str()).collect();
        assert!(after_root.contains(&"a"));
        assert!(after_root.contains(&"b"));
    }

    #[test]
    fn topo_sort_fan_in() {
        let nodes = vec![
            DagNodeDesign {
                id: "a".into(),
                capability_ids: vec![],
                task_context: "".into(),
                arguments: None,
                depends_on: vec![],
                timeout_seconds: 600,
            },
            DagNodeDesign {
                id: "b".into(),
                capability_ids: vec![],
                task_context: "".into(),
                arguments: None,
                depends_on: vec![],
                timeout_seconds: 600,
            },
            DagNodeDesign {
                id: "c".into(),
                capability_ids: vec![],
                task_context: "".into(),
                arguments: None,
                depends_on: vec!["a".into(), "b".into()],
                timeout_seconds: 600,
            },
        ];
        let order = topological_sort(&nodes).unwrap();
        assert_eq!(order[2], "c");
    }

    #[test]
    fn topo_sort_detects_cycle() {
        let nodes = vec![
            DagNodeDesign {
                id: "a".into(),
                capability_ids: vec![],
                task_context: "".into(),
                arguments: None,
                depends_on: vec!["b".into()],
                timeout_seconds: 600,
            },
            DagNodeDesign {
                id: "b".into(),
                capability_ids: vec![],
                task_context: "".into(),
                arguments: None,
                depends_on: vec!["a".into()],
                timeout_seconds: 600,
            },
        ];
        assert!(topological_sort(&nodes).is_err());
    }

    #[test]
    fn topo_sort_unknown_dependency() {
        let nodes = vec![DagNodeDesign {
            id: "a".into(),
            capability_ids: vec![],
            task_context: "".into(),
            arguments: None,
            depends_on: vec!["nonexistent".into()],
            timeout_seconds: 600,
        }];
        assert!(topological_sort(&nodes).is_err());
    }

    #[test]
    fn topo_sort_single_node() {
        let nodes = vec![DagNodeDesign {
            id: "only".into(),
            capability_ids: vec![],
            task_context: "".into(),
            arguments: None,
            depends_on: vec![],
            timeout_seconds: 600,
        }];
        let order = topological_sort(&nodes).unwrap();
        assert_eq!(order, vec!["only"]);
    }

    #[test]
    fn select_prompt_normal() {
        let p = select_prompt("normal", Path::new("prompts"));
        assert!(
            p.contains("the Execution Platform"),
            "normal prompt should contain 'the Execution Platform', got: {p}"
        );
    }

    #[test]
    fn select_prompt_triggered() {
        let p = select_prompt("triggered", Path::new("prompts"));
        assert!(
            p.contains("triggered"),
            "triggered prompt should contain 'triggered', got: {p}"
        );
    }

    #[test]
    fn select_prompt_scheduled() {
        let p = select_prompt("scheduled", Path::new("prompts"));
        assert!(
            p.contains("scheduled"),
            "scheduled prompt should contain 'scheduled', got: {p}"
        );
    }

    #[test]
    fn select_prompt_dag() {
        let p = select_prompt("dag", Path::new("prompts"));
        assert!(
            p.contains("depends_on"),
            "dag prompt should contain 'depends_on', got: {p}"
        );
    }

    #[test]
    fn select_prompt_unknown_defaults_to_normal() {
        let p = select_prompt(UNRECOGNIZED_KIND, Path::new("prompts"));
        assert!(
            p.contains("the Execution Platform"),
            "unknown kind should still get prompt, got: {p}"
        );
    }

    #[test]
    fn infer_normal_from_plain_goal() {
        let kind = infer_template_kind("do something", &[]);
        assert_eq!(kind, "normal");
    }

    #[test]
    fn infer_dag_from_goal() {
        let kind = infer_template_kind("run dag multi-step pipeline", &[]);
        assert_eq!(kind, "dag");
    }

    #[test]
    fn infer_dag_from_constraint() {
        let kind = infer_template_kind("build", &["multi-step workflow".into()]);
        assert_eq!(kind, "dag");
    }

    #[test]
    fn infer_triggered_from_constraint() {
        let kind = infer_template_kind("handle event", &["triggered by webhook".to_string()]);
        assert_eq!(kind, "triggered");
    }

    #[test]
    fn infer_triggered_from_goal() {
        let kind = infer_template_kind("webhook triggered task", &[]);
        assert_eq!(kind, "triggered");
    }

    #[test]
    fn infer_scheduled_from_constraint() {
        let kind = infer_template_kind("daily report", &["scheduled: cron 0 9 * * *".to_string()]);
        assert_eq!(kind, "scheduled");
    }

    #[test]
    fn infer_scheduled_from_goal() {
        let kind = infer_template_kind("periodic health check", &[]);
        assert_eq!(kind, "scheduled");
    }

    #[test]
    fn build_prompt_contains_goal_and_constraints() {
        let prompt = build_execution_prompt(
            "normal",
            "test goal",
            &["c1".to_string(), "c2".to_string()],
            Some(Path::new("prompts")),
            &[],
            None,
        );
        assert!(prompt.contains("test goal"));
        assert!(prompt.contains("c1"));
        assert!(prompt.contains("c2"));
        assert!(prompt.contains("the Execution Platform"));
    }

    #[test]
    fn build_prompt_empty_constraints() {
        let prompt = build_execution_prompt(
            "triggered",
            "goal",
            &[],
            Some(Path::new("prompts")),
            &[],
            None,
        );
        assert!(prompt.contains("none"));
        assert!(prompt.contains("the Execution Platform"));
    }

    #[test]
    fn build_dag_prompt_contains_dag_specifics() {
        let prompt = build_execution_prompt(
            "dag",
            "multi-step task",
            &[],
            Some(Path::new("prompts")),
            &[],
            None,
        );
        assert!(prompt.contains("depends_on"));
        assert!(prompt.contains("the Execution Platform"));
    }

    #[test]
    fn subagent_design_json_roundtrip() {
        let design = SubAgentDesign {
            template_kind: "normal".to_string(),
            capability_ids: vec!["shell.exec".to_string(), "http.request".to_string()],
            task_context: "build and deploy".to_string(),
            arguments: None,
            max_turns: 10,
            timeout_seconds: 600,
        };
        let json = serde_json::to_string(&design).unwrap();
        let back: SubAgentDesign = serde_json::from_str(&json).unwrap();
        assert_eq!(back.template_kind, design.template_kind);
        assert_eq!(back.capability_ids, design.capability_ids);
        assert_eq!(back.task_context, design.task_context);
        assert_eq!(back.max_turns, design.max_turns);
        assert_eq!(back.timeout_seconds, design.timeout_seconds);
    }

    #[test]
    fn dag_design_json_roundtrip() {
        let design = DagDesign {
            template_kind: "dag".to_string(),
            nodes: vec![
                DagNodeDesign {
                    id: "n1".into(),
                    capability_ids: vec!["shell.exec".into()],
                    task_context: "step 1".into(),
                    arguments: None,
                    depends_on: vec![],
                    timeout_seconds: 600,
                },
                DagNodeDesign {
                    id: "n2".into(),
                    capability_ids: vec!["http.request".into()],
                    task_context: "step 2".into(),
                    arguments: None,
                    depends_on: vec!["n1".into()],
                    timeout_seconds: 300,
                },
            ],
        };
        let json = serde_json::to_string(&design).unwrap();
        let back: DagDesign = serde_json::from_str(&json).unwrap();
        assert_eq!(back.nodes.len(), 2);
        assert_eq!(back.nodes[0].id, "n1");
        assert_eq!(back.nodes[1].depends_on, vec!["n1"]);
        assert_eq!(back.nodes[1].timeout_seconds, 300);
    }

    #[test]
    fn all_prompts_contain_output_format() {
        for kind in &["normal", "triggered", "scheduled", "dag"] {
            let p = select_prompt(kind, Path::new("prompts"));
            assert!(!p.is_empty(), "prompt for {kind} should not be empty");
            assert!(
                p.contains("Output Format"),
                "prompt {kind} missing 'Output Format'"
            );
            assert!(
                p.contains("```json"),
                "prompt {kind} missing json code block"
            );
            assert!(
                p.contains("capability_ids"),
                "prompt {kind} missing capability_ids"
            );
            assert!(
                p.contains("task_context"),
                "prompt {kind} missing task_context"
            );
        }
    }

    #[test]
    fn dag_prompt_contains_depends_on() {
        let p = select_prompt("dag", Path::new("prompts"));
        assert!(p.contains("depends_on"));
        assert!(
            p.contains("TaskFlow"),
            "dag prompt should contain 'TaskFlow'"
        );
    }

    #[test]
    fn parse_empty_dag_falls_back_to_single() {
        let json = r#"{"template_kind":"dag","nodes":[]}"#;
        let result = parse_execution_design(json);

        assert!(result.is_err());
    }

    #[test]
    fn parse_dag_with_single_node() {
        let json = r#"{"template_kind":"dag","nodes":[{"id":"only","capability_ids":["shell.exec"],"task_context":"just one","depends_on":[]}]}"#;
        let design = parse_execution_design(json).unwrap();
        match design {
            ExecutionDesign::Dag(dag) => {
                assert_eq!(dag.nodes.len(), 1);
                assert_eq!(dag.nodes[0].id, "only");
            }
            _ => panic!("expected Dag"),
        }
    }
}
