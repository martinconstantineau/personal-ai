//! Tool protocol. Tools are first-class objects with declared schemas,
//! permissions, and risk levels — the model sees schemas, never raw access.
//!
//! Adding a tool: implement [`Tool`], register it on [`ToolRegistry`].
//! The runtime enforces `required_permissions` before `execute` ever runs.

use async_trait::async_trait;
use pai_core::*;
use pai_memory::MemoryItem;
use pai_permissions::{Permission, RiskLevel};
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Static description of a tool — what the model and the policy engine see.
#[derive(Debug, Clone, Serialize)]
pub struct ToolDescriptor {
    pub name: String,
    pub description: String,
    pub version: String,
    /// JSON Schema for arguments.
    pub input_schema: serde_json::Value,
    pub output_schema: serde_json::Value,
    /// Permissions required before execution. Enforced by the runtime.
    pub required_permissions: Vec<Permission>,
    pub risk: RiskLevel,
    /// Where this tool may run.
    pub execution: ExecutionMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    /// Pure computation — no side effects, safe to auto-run.
    Local,
    /// Touches external state (email, files) — respect risk level.
    SideEffecting,
    /// Long-running; returns a job handle.
    Background,
}

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub value: serde_json::Value,
    /// Short human-readable summary for audit + approval prompts.
    pub summary: String,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn descriptor(&self) -> ToolDescriptor;
    /// Validate + run. Arguments were already schema-validated by the runtime.
    async fn execute<'x>(
        &self,
        args: serde_json::Value,
        ctx: &'x ToolContext<'x>,
    ) -> Result<ToolOutput>;
}

/// Everything a tool is allowed to see. Deliberately narrow: tools get
/// capabilities through this context, never ambient system access.
pub struct ToolContext<'a> {
    pub run: AgentRunId,
    pub device: DeviceId,
    pub memory: Option<&'a dyn pai_memory::MemoryBackend>,
    /// When the run's conversation has isolated memory, this is `Some(conv)`:
    /// memory writes are tagged to that conversation and memory reads stay
    /// inside its scope. `None` = shared (global) memory.
    pub memory_scope: Option<ConversationId>,
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.descriptor().name.clone(), tool);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn descriptors(&self) -> Vec<ToolDescriptor> {
        self.tools.values().map(|t| t.descriptor()).collect()
    }
}

/// Validate `args` against a (subset of) JSON Schema: type + required fields.
/// Enough for the protocol; a full JSON Schema validator can slot in later.
pub fn validate_args(schema: &serde_json::Value, args: &serde_json::Value) -> Result<()> {
    if schema.get("type").and_then(|t| t.as_str()) == Some("object") && !args.is_object() {
        return Err(Error::InvalidInput("tool args must be an object".into()));
    }
    if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
        for field in required.iter().filter_map(|f| f.as_str()) {
            if args.get(field).is_none() {
                return Err(Error::InvalidInput(format!("missing arg '{field}'")));
            }
        }
    }
    if let Some(props) = schema.get("properties").and_then(|p| p.as_object()) {
        for (k, v) in props {
            if let Some(arg) = args.get(k) {
                let ok = match v.get("type").and_then(|t| t.as_str()) {
                    Some("number") | Some("integer") => arg.is_number(),
                    Some("string") => arg.is_string(),
                    Some("boolean") => arg.is_boolean(),
                    Some("array") => arg.is_array(),
                    Some("object") => arg.is_object(),
                    _ => true,
                };
                if !ok {
                    return Err(Error::InvalidInput(format!("arg '{k}' wrong type")));
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Built-ins
// ---------------------------------------------------------------------------

/// `calculator.add` — the canonical safe demo tool. Pure, local, always-allow.
pub struct CalculatorAdd;

#[async_trait]
impl Tool for CalculatorAdd {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "calculator.add".into(),
            description: "Add two numbers.".into(),
            version: "1.0.0".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "a": {"type": "number"},
                    "b": {"type": "number"}
                },
                "required": ["a", "b"]
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {"result": {"type": "number"}},
                "required": ["result"]
            }),
            required_permissions: vec![],
            risk: RiskLevel::Low,
            execution: ExecutionMode::Local,
        }
    }

    async fn execute<'x>(
        &self,
        args: serde_json::Value,
        _ctx: &'x ToolContext<'x>,
    ) -> Result<ToolOutput> {
        let a = args["a"]
            .as_f64()
            .ok_or_else(|| Error::InvalidInput("a".into()))?;
        let b = args["b"]
            .as_f64()
            .ok_or_else(|| Error::InvalidInput("b".into()))?;
        Ok(ToolOutput {
            value: serde_json::json!({"result": a + b}),
            summary: format!("{a} + {b} = {}", a + b),
        })
    }
}

/// `memory.remember` — store a user-stated fact as semantic memory.
pub struct MemoryRemember;

#[async_trait]
impl Tool for MemoryRemember {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "memory.remember".into(),
            description: "Store a fact the user states as long-term semantic memory.".into(),
            version: "1.0.0".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "content": {"type": "string"},
                    "memory_type": {"type": "string", "enum":
                        ["semantic", "episodic", "procedural", "relationship"]},
                    "importance": {"type": "number"}
                },
                "required": ["content"]
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {"memory_id": {"type": "string"}},
                "required": ["memory_id"]
            }),
            required_permissions: vec![Permission::MemoryWrite],
            risk: RiskLevel::Low,
            execution: ExecutionMode::Local,
        }
    }

    async fn execute<'x>(
        &self,
        args: serde_json::Value,
        ctx: &'x ToolContext<'x>,
    ) -> Result<ToolOutput> {
        let mem = ctx
            .memory
            .ok_or_else(|| Error::Other("memory backend unavailable".into()))?;
        let content = args["content"]
            .as_str()
            .ok_or_else(|| Error::InvalidInput("content".into()))?;
        let scope = match args["memory_type"].as_str().unwrap_or("semantic") {
            "episodic" => MemoryScope::Episodic,
            "procedural" => MemoryScope::Procedural,
            "relationship" => MemoryScope::Relationship,
            _ => MemoryScope::Semantic,
        };
        let item = MemoryItem {
            scope,
            importance: args["importance"].as_f64().unwrap_or(0.8) as f32,
            conversation: ctx.memory_scope,
            ..pai_memory::user_fact(content, 0.8)
        };
        mem.put(&item).await?;
        Ok(ToolOutput {
            value: serde_json::json!({"memory_id": item.id.to_string()}),
            summary: format!("remembered: {content}"),
        })
    }
}

/// `memory.forget` — soft-delete a memory by id, or by the top text match.
/// Deletion is destructive: it requires [`Permission::MemoryDelete`], which
/// defaults to `AskUser` so the model can never erase data silently.
pub struct MemoryForget;

#[async_trait]
impl Tool for MemoryForget {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "memory.forget".into(),
            description: "Forget (delete) a memory — by exact memory_id, or by \
                          a text query matching the memory's content."
                .into(),
            version: "1.0.0".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "memory_id": {"type": "string"},
                    "query": {"type": "string"}
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "deleted_id": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["deleted_id"]
            }),
            required_permissions: vec![Permission::MemoryDelete],
            risk: RiskLevel::Medium,
            execution: ExecutionMode::Local,
        }
    }

    async fn execute<'x>(
        &self,
        args: serde_json::Value,
        ctx: &'x ToolContext<'x>,
    ) -> Result<ToolOutput> {
        let mem = ctx
            .memory
            .ok_or_else(|| Error::Other("memory backend unavailable".into()))?;

        let item = if let Some(id) = args["memory_id"].as_str() {
            let id = uuid::Uuid::parse_str(id)
                .map_err(|_| Error::InvalidInput("memory_id must be a uuid".into()))?;
            let item = mem.get(MemoryId(id)).await?;
            mem.delete(item.id).await?;
            item
        } else if let Some(q) = args["query"].as_str() {
            let hits = mem
                .recall(&pai_memory::RecallQuery {
                    text: Some(q.to_string()),
                    limit: 1,
                    // Forget only what this run is allowed to see.
                    memory_scope: match ctx.memory_scope {
                        Some(c) => pai_memory::MemoryScopeQuery::Scoped(c, true),
                        None => pai_memory::MemoryScopeQuery::GlobalOnly,
                    },
                    ..Default::default()
                })
                .await?;
            let scored = hits
                .into_iter()
                .next()
                .ok_or_else(|| Error::NotFound(format!("no memory matching '{q}'")))?;
            mem.delete(scored.item.id).await?;
            scored.item
        } else {
            return Err(Error::InvalidInput(
                "memory.forget needs 'memory_id' or 'query'".into(),
            ));
        };

        Ok(ToolOutput {
            value: serde_json::json!({
                "deleted_id": item.id.to_string(),
                "content": item.content,
            }),
            summary: format!("forgot: {}", item.content),
        })
    }
}

/// A registry pre-loaded with the safe built-ins.
pub fn builtin_registry() -> ToolRegistry {
    let mut r = ToolRegistry::default();
    r.register(Arc::new(CalculatorAdd));
    r.register(Arc::new(MemoryRemember));
    r.register(Arc::new(MemoryForget));
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn calculator_add_validates_and_runs() {
        let tool = CalculatorAdd;
        let d = tool.descriptor();
        validate_args(&d.input_schema, &serde_json::json!({"a":2,"b":3})).unwrap();
        assert!(validate_args(&d.input_schema, &serde_json::json!({"a":2})).is_err());
        let ctx = ToolContext {
            run: AgentRunId::new(),
            device: DeviceId::new(),
            memory: None,
            memory_scope: None,
        };
        let out = tool
            .execute(serde_json::json!({"a":2,"b":3}), &ctx)
            .await
            .unwrap();
        assert_eq!(out.value["result"], 5.0);
    }
}
