//! Subscription policy: file-backed approvals and the default policy that
//! gates which event categories socket clients are allowed to subscribe to.

use std::cell::RefCell;
use std::error::Error;
use std::path::PathBuf;
use std::{fmt, fs};

use serde::{Deserialize, Serialize};
use tau_proto::EventSelector;

use crate::connection::{ConnectionMetadata, ConnectionOrigin};
use crate::session_store::SessionStoreError;

/// Persisted approval for one subscription request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SubscriptionApproval {
    pub connection_name: String,
    pub connection_origin: ConnectionOrigin,
    pub selectors: Vec<EventSelector>,
}

/// File-backed store of approved subscription sets.
#[derive(Debug)]
pub struct PolicyStore {
    path: PathBuf,
    approvals: Vec<SubscriptionApproval>,
}

impl PolicyStore {
    /// Opens a policy store, loading any existing approvals.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, SessionStoreError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| {
                SessionStoreError::CreateParentDirectory {
                    path: parent.to_path_buf(),
                    source,
                }
            })?;
        }

        let approvals = if path.exists() {
            let bytes = fs::read(&path).map_err(|source| SessionStoreError::Read {
                path: path.clone(),
                source,
            })?;
            if bytes.is_empty() {
                Vec::new()
            } else {
                ciborium::from_reader(bytes.as_slice()).map_err(|source| {
                    SessionStoreError::Decode {
                        path: path.clone(),
                        source,
                    }
                })?
            }
        } else {
            Vec::new()
        };

        Ok(Self { path, approvals })
    }

    /// Returns true when the exact approval is already present.
    #[must_use]
    pub fn contains(&self, approval: &SubscriptionApproval) -> bool {
        self.approvals.iter().any(|existing| existing == approval)
    }

    /// Records one approval and persists it if it is new.
    pub fn record(&mut self, approval: SubscriptionApproval) -> Result<(), SessionStoreError> {
        if self.contains(&approval) {
            return Ok(());
        }
        self.approvals.push(approval);

        let mut encoded = Vec::new();
        ciborium::into_writer(&self.approvals, &mut encoded).map_err(|source| {
            SessionStoreError::Encode {
                path: self.path.clone(),
                source,
            }
        })?;
        fs::write(&self.path, encoded).map_err(|source| SessionStoreError::Write {
            path: self.path.clone(),
            source,
        })
    }

    /// Returns all persisted approvals.
    #[must_use]
    pub fn approvals(&self) -> &[SubscriptionApproval] {
        &self.approvals
    }
}

/// Policy error returned when a subscription request is rejected.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionPolicyError {
    reason: String,
}

impl SubscriptionPolicyError {
    /// Creates a new policy error.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    /// Returns the rejection reason.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl fmt::Display for SubscriptionPolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason)
    }
}

impl Error for SubscriptionPolicyError {}

/// Subscription-time policy hook.
pub trait SubscriptionPolicy {
    fn evaluate(
        &self,
        connection: &ConnectionMetadata,
        selectors: &[EventSelector],
    ) -> Result<(), SubscriptionPolicyError>;
}

/// Default MVP subscription policy.
#[derive(Debug, Default)]
pub struct DefaultSubscriptionPolicy {
    store: Option<RefCell<PolicyStore>>,
}

impl DefaultSubscriptionPolicy {
    /// Creates the default in-memory-only policy.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates the default policy backed by one approval store.
    pub fn with_store(store: PolicyStore) -> Self {
        Self {
            store: Some(RefCell::new(store)),
        }
    }

    fn record_approval(
        &self,
        connection: &ConnectionMetadata,
        selectors: &[EventSelector],
    ) -> Result<(), SubscriptionPolicyError> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        if connection.origin != ConnectionOrigin::Socket {
            return Ok(());
        }

        store
            .borrow_mut()
            .record(SubscriptionApproval {
                connection_name: connection.name.clone(),
                connection_origin: connection.origin.clone(),
                selectors: selectors.to_vec(),
            })
            .map_err(|error| SubscriptionPolicyError::new(error.to_string()))
    }
}

impl SubscriptionPolicy for DefaultSubscriptionPolicy {
    fn evaluate(
        &self,
        connection: &ConnectionMetadata,
        selectors: &[EventSelector],
    ) -> Result<(), SubscriptionPolicyError> {
        if connection.origin == ConnectionOrigin::Socket {
            // Closed list of categories a socket client is allowed to
            // subscribe to. The unknown `EventCategory::Other` and the
            // wire-level `Wire` family are rejected outright — UI
            // clients should not see at-least-once envelope plumbing.
            fn category_allowed(category: &tau_proto::EventCategory) -> bool {
                use tau_proto::EventCategory as C;
                matches!(
                    category,
                    C::Tool
                        | C::Extension
                        | C::Agent
                        | C::Session
                        | C::Ui
                        | C::Harness
                        | C::Shell
                        | C::Term
                )
            }
            for selector in selectors {
                let allowed = match selector {
                    EventSelector::Exact(name) => category_allowed(&name.category),
                    EventSelector::Prefix(prefix) => {
                        // The category portion of the prefix must
                        // resolve to a known, allowed category.
                        let category_str = prefix.split_once('.').map(|(c, _)| c).unwrap_or(prefix);
                        let category = tau_proto::EventCategory::from_wire(category_str);
                        category_allowed(&category)
                    }
                };
                if !allowed {
                    return Err(SubscriptionPolicyError::new(
                        "socket clients may only subscribe to allowed event families",
                    ));
                }
            }
        }

        self.record_approval(connection, selectors)
    }
}
