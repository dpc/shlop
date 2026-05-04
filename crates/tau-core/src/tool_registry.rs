//! Live tool registration tracking and routing of `tool.request` events to
//! the connection that owns each tool.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;

use tau_proto::{ConnectionId, Event, ToolName, ToolRequest, ToolSpec};

use crate::bus::EventBus;
use crate::connection::{RouteError, RouteReport};

/// One live provider registered for a tool name.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolProvider {
    pub connection_id: ConnectionId,
    pub tool: ToolSpec,
}

/// Warning emitted by the tool registry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolRegistryWarning {
    DuplicateRegistration {
        tool_name: ToolName,
        existing_provider_ids: Vec<ConnectionId>,
    },
}

/// Summary of one registration call.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RegisterToolReport {
    pub warnings: Vec<ToolRegistryWarning>,
}

/// Error returned when a tool request cannot be routed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolRouteError {
    NoProvider { tool_name: ToolName },
    Route(RouteError),
}

impl fmt::Display for ToolRouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoProvider { tool_name } => write!(f, "no live provider for tool: {tool_name}"),
            Self::Route(error) => write!(f, "failed to route tool request: {error}"),
        }
    }
}

impl Error for ToolRouteError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::NoProvider { .. } => None,
            Self::Route(error) => Some(error),
        }
    }
}

/// Summary of one `tool.request` routing decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolRouteReport {
    pub provider_connection_id: ConnectionId,
    pub route_report: RouteReport,
}

/// Live tool registration state keyed by connection and tool name.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolRegistry {
    providers_by_tool: HashMap<ToolName, Vec<ToolProvider>>,
    tools_by_connection: HashMap<ConnectionId, Vec<ToolName>>,
}

impl ToolRegistry {
    /// Creates an empty tool registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers one tool for a live provider connection.
    pub fn register(&mut self, connection_id: &str, tool: ToolSpec) -> RegisterToolReport {
        let tool_name = tool.name.clone();
        let providers = self.providers_by_tool.entry(tool_name.clone()).or_default();

        let existing_provider_ids = providers
            .iter()
            .map(|provider| provider.connection_id.clone())
            .collect::<Vec<_>>();
        let mut report = RegisterToolReport::default();
        if !existing_provider_ids.is_empty() {
            report
                .warnings
                .push(ToolRegistryWarning::DuplicateRegistration {
                    tool_name: tool_name.clone(),
                    existing_provider_ids,
                });
        }

        if let Some(existing_provider) = providers
            .iter_mut()
            .find(|provider| provider.connection_id == connection_id)
        {
            existing_provider.tool = tool;
        } else {
            providers.push(ToolProvider {
                connection_id: connection_id.into(),
                tool,
            });
        }

        let connection_tools = self
            .tools_by_connection
            .entry(connection_id.into())
            .or_default();
        if !connection_tools.contains(&tool_name) {
            connection_tools.push(tool_name);
        }

        report
    }

    /// Unregisters one tool from one provider connection.
    pub fn unregister(&mut self, connection_id: &str, tool_name: &str) -> bool {
        let mut removed = false;

        if let Some(providers) = self.providers_by_tool.get_mut(tool_name) {
            let initial_len = providers.len();
            providers.retain(|provider| provider.connection_id != connection_id);
            removed = providers.len() != initial_len;
            if providers.is_empty() {
                self.providers_by_tool.remove(tool_name);
            }
        }

        if removed {
            self.remove_tool_from_connection(connection_id, tool_name);
        }

        removed
    }

    /// Unregisters all tools owned by one disconnected provider connection.
    pub fn unregister_connection(&mut self, connection_id: &str) -> Vec<ToolName> {
        let Some(tool_names) = self.tools_by_connection.remove(connection_id) else {
            return Vec::new();
        };

        for tool_name in &tool_names {
            if let Some(providers) = self.providers_by_tool.get_mut(tool_name) {
                providers.retain(|provider| provider.connection_id != connection_id);
                if providers.is_empty() {
                    self.providers_by_tool.remove(tool_name);
                }
            }
        }

        tool_names
    }

    /// Returns all currently live providers for a tool name.
    #[must_use]
    pub fn providers_for(&self, tool_name: &str) -> Vec<ToolProvider> {
        self.providers_by_tool
            .get(tool_name)
            .cloned()
            .unwrap_or_default()
    }

    /// Returns all unique tool names currently registered.
    #[must_use]
    pub fn all_tool_names(&self) -> Vec<&ToolName> {
        self.providers_by_tool.keys().collect()
    }

    /// Returns all unique tool specs, one per tool name (first provider wins).
    #[must_use]
    pub fn all_tools(&self) -> Vec<&ToolSpec> {
        let mut tools: Vec<_> = self
            .providers_by_tool
            .values()
            .filter_map(|providers| providers.first().map(|p| &p.tool))
            .collect();
        tools.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        tools
    }

    /// Picks one currently live provider for a tool name.
    #[must_use]
    pub fn resolve_provider(&self, tool_name: &str) -> Option<&ToolProvider> {
        self.providers_by_tool
            .get(tool_name)
            .and_then(|providers| providers.first())
    }

    /// Routes a `tool.request` to one live provider as a directed
    /// `tool.invoke`.
    pub fn route_tool_request(
        &self,
        bus: &mut EventBus,
        requester_id: &str,
        request: ToolRequest,
    ) -> Result<ToolRouteReport, ToolRouteError> {
        let provider_connection_id = self
            .resolve_provider(&request.tool_name)
            .map(|provider| provider.connection_id.clone())
            .ok_or_else(|| ToolRouteError::NoProvider {
                tool_name: request.tool_name.clone(),
            })?;

        let route_report = bus
            .send_to(
                &provider_connection_id,
                Some(requester_id),
                Event::ToolInvoke(tau_proto::ToolInvoke {
                    call_id: request.call_id,
                    tool_name: request.tool_name,
                    arguments: request.arguments,
                }),
            )
            .map_err(ToolRouteError::Route)?;

        Ok(ToolRouteReport {
            provider_connection_id,
            route_report,
        })
    }

    fn remove_tool_from_connection(&mut self, connection_id: &str, tool_name: &str) {
        if let Some(tool_names) = self.tools_by_connection.get_mut(connection_id) {
            tool_names.retain(|name| name != tool_name);
            if tool_names.is_empty() {
                self.tools_by_connection.remove(connection_id);
            }
        }
    }
}
