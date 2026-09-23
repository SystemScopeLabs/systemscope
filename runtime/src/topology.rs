//! Topology construction and elaboration (`docs/m0-design.md` §6).
//!
//! A [`TopologyBuilder`] records clock domains, components, and links in declaration
//! order. [`TopologyBuilder::elaborate`] validates the whole graph once and produces a
//! [`Runtime`] in the `Elaborated` state. Declaration order fixes every id.

use std::collections::BTreeMap;
use std::fmt;

use systemscope_contracts::component::{Component, ComponentId, PortId, PortSpec};
use systemscope_contracts::time::{
    ClockDomain, ClockDomainId, Frequency, Rounding, SimulationClock, Tick, TimeError,
};
use systemscope_contracts::topology::LinkLatency;

use crate::runtime::{Peer, Runtime, SessionConfig, Slot};

/// Why a topology could not be elaborated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ElaborationError {
    /// Two components share a path.
    DuplicatePath(String),
    /// A component declares two ports with the same name.
    DuplicatePortName(String),
    /// A component declares more ports than a [`PortId`] can index.
    TooManyPorts(String),
    /// A link names a component id that was not declared.
    UnknownComponent(ComponentId),
    /// A link names a port its component does not declare.
    UnknownPort(String),
    /// A link joins two initiators or two targets.
    RoleMismatch(String, String),
    /// A link joins ports of different protocols or protocol versions.
    ProtocolMismatch(String, String),
    /// A port appears in more than one link.
    PortAlreadyLinked(String),
    /// A declared port has no link.
    UnconnectedPort(String),
    /// A link latency names an undeclared clock domain.
    UnknownClockDomain(ClockDomainId),
}

impl fmt::Display for ElaborationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ElaborationError::DuplicatePath(p) => write!(f, "duplicate component path {p}"),
            ElaborationError::DuplicatePortName(p) => write!(f, "duplicate port {p}"),
            ElaborationError::TooManyPorts(c) => write!(f, "too many ports on {c}"),
            ElaborationError::UnknownComponent(id) => write!(f, "unknown component {}", id.0),
            ElaborationError::UnknownPort(p) => write!(f, "unknown port {p}"),
            ElaborationError::RoleMismatch(a, b) => {
                write!(f, "link {a} <-> {b} must join an initiator and a target")
            }
            ElaborationError::ProtocolMismatch(a, b) => {
                write!(f, "link {a} <-> {b} joins different protocols")
            }
            ElaborationError::PortAlreadyLinked(p) => write!(f, "port {p} is linked twice"),
            ElaborationError::UnconnectedPort(p) => write!(f, "port {p} is not connected"),
            ElaborationError::UnknownClockDomain(id) => {
                write!(f, "unknown clock domain {}", id.0)
            }
        }
    }
}

impl std::error::Error for ElaborationError {}

struct Declared {
    path: String,
    component: Box<dyn Component>,
}

struct LinkDecl {
    a: (ComponentId, String),
    b: (ComponentId, String),
    latency: Option<LinkLatency>,
}

/// Declares a topology in a fixed order.
pub struct TopologyBuilder {
    clock: SimulationClock,
    domains: Vec<ClockDomain>,
    components: Vec<Declared>,
    links: Vec<LinkDecl>,
}

impl TopologyBuilder {
    /// Starts a topology for a session with the given tick resolution.
    pub fn new(clock: SimulationClock) -> TopologyBuilder {
        TopologyBuilder {
            clock,
            domains: Vec::new(),
            components: Vec::new(),
            links: Vec::new(),
        }
    }

    /// Declares a clock domain. Ids are assigned in declaration order.
    pub fn add_clock(
        &mut self,
        frequency: Frequency,
        offset: Tick,
        edge_rounding: Rounding,
    ) -> Result<ClockDomainId, TimeError> {
        let id = ClockDomainId(
            u32::try_from(self.domains.len()).expect("fewer than 2^32 clock domains"),
        );
        let domain = ClockDomain::new(&self.clock, id, frequency, offset, edge_rounding)?;
        self.domains.push(domain);
        Ok(id)
    }

    /// Declares a component. Ids are assigned in declaration order.
    pub fn add_component(
        &mut self,
        path: impl Into<String>,
        component: Box<dyn Component>,
    ) -> ComponentId {
        let id = ComponentId(
            u32::try_from(self.components.len())
                .ok()
                .filter(|&n| n != ComponentId::RUNTIME.0)
                .expect("fewer than 2^32 - 1 components"),
        );
        self.components.push(Declared {
            path: path.into(),
            component,
        });
        id
    }

    /// Links two ports, named by component and port name. Validated at elaboration.
    pub fn connect(
        &mut self,
        a: (ComponentId, &str),
        b: (ComponentId, &str),
        latency: Option<LinkLatency>,
    ) {
        self.links.push(LinkDecl {
            a: (a.0, a.1.to_owned()),
            b: (b.0, b.1.to_owned()),
            latency,
        });
    }

    /// Validates the topology and builds a runtime in the `Elaborated` state.
    pub fn elaborate(self, config: SessionConfig) -> Result<Runtime, ElaborationError> {
        let mut paths = BTreeMap::new();
        let mut slots = Vec::with_capacity(self.components.len());
        for declared in self.components {
            if paths.insert(declared.path.clone(), ()).is_some() {
                return Err(ElaborationError::DuplicatePath(declared.path));
            }
            let ports = declared.component.ports();
            if u16::try_from(ports.len()).is_err() {
                return Err(ElaborationError::TooManyPorts(declared.path));
            }
            let mut names = BTreeMap::new();
            for spec in &ports {
                if names.insert(spec.name, ()).is_some() {
                    return Err(ElaborationError::DuplicatePortName(format!(
                        "{}.{}",
                        declared.path, spec.name
                    )));
                }
            }
            slots.push(Slot {
                path: declared.path,
                ports,
                component: declared.component,
            });
        }

        let mut peers: Vec<Vec<Option<Peer>>> =
            slots.iter().map(|s| vec![None; s.ports.len()]).collect();
        for link in &self.links {
            if let Some(LinkLatency::Cycles { domain, .. }) = link.latency
                && domain.0 as usize >= self.domains.len()
            {
                return Err(ElaborationError::UnknownClockDomain(domain));
            }
            let (a, a_spec, a_name) = resolve_port(&slots, &link.a)?;
            let (b, b_spec, b_name) = resolve_port(&slots, &link.b)?;
            if a_spec.role == b_spec.role {
                return Err(ElaborationError::RoleMismatch(a_name, b_name));
            }
            if a_spec.protocol != b_spec.protocol {
                return Err(ElaborationError::ProtocolMismatch(a_name, b_name));
            }
            for ((component, port), name) in [(a, &a_name), (b, &b_name)] {
                if peers[component.0 as usize][usize::from(port.0)].is_some() {
                    return Err(ElaborationError::PortAlreadyLinked(name.clone()));
                }
            }
            peers[a.0.0 as usize][usize::from(a.1.0)] = Some(Peer {
                component: b.0,
                port: b.1,
                latency: link.latency,
            });
            peers[b.0.0 as usize][usize::from(b.1.0)] = Some(Peer {
                component: a.0,
                port: a.1,
                latency: link.latency,
            });
        }

        for (slot, slot_peers) in slots.iter().zip(&peers) {
            for (spec, peer) in slot.ports.iter().zip(slot_peers) {
                if peer.is_none() {
                    return Err(ElaborationError::UnconnectedPort(format!(
                        "{}.{}",
                        slot.path, spec.name
                    )));
                }
            }
        }

        let peers = peers
            .into_iter()
            .map(|ps| ps.into_iter().map(|p| p.expect("checked above")).collect())
            .collect();
        Ok(Runtime::from_elaboration(
            self.clock,
            self.domains,
            slots,
            peers,
            config,
        ))
    }
}

/// Finds a port by name, returning its ids, spec, and a `path.port` label.
fn resolve_port<'a>(
    slots: &'a [Slot],
    (component, name): &(ComponentId, String),
) -> Result<((ComponentId, PortId), &'a PortSpec, String), ElaborationError> {
    let slot = slots
        .get(component.0 as usize)
        .ok_or(ElaborationError::UnknownComponent(*component))?;
    let label = format!("{}.{}", slot.path, name);
    let index = slot
        .ports
        .iter()
        .position(|p| p.name == name)
        .ok_or_else(|| ElaborationError::UnknownPort(label.clone()))?;
    let port = PortId(u16::try_from(index).expect("port count checked"));
    Ok(((*component, port), &slot.ports[index], label))
}
