//! Who holds a VIP, decided without I/O (so it is tested): one Lease per VIP
//! (coordination.k8s.io/v1, in the controller's namespace), the `vip` sidecar of
//! each fleet pod asks what to do now from what it knows (docs/DESIGN-v0.4.x.md 7.).
//!
//! - A pod takes a free Lease (no holder) or one whose holder has not renewed it
//!   for its `leaseDurationSeconds`, as this pod observed it (its own clock: the
//!   nodes' clocks are not compared), and only while its rproxy is ready
//!   (`/readyz`) and the controller has applied the rule sets to it (the readiness
//!   gate). A pod that holds other VIPs waits a little longer for each (VIPs spread
//!   over the nodes), a cordoned node much longer (it takes one only when no other
//!   node does).
//! - The holder renews every `renew`, and retries every `retry` when that fails.
//! - It releases (removes the address, then clears the holder so another pod takes
//!   it at once) on shutdown, when rproxy is not ready (draining included), when its
//!   node is cordoned or no longer selected, and when another MAC announces the VIP
//!   while it could not renew (another pod may hold the Lease).
//! - When the API server cannot be reached it keeps the VIP while rproxy is ready
//!   (`hold`), or drops it once the Lease would have expired (`release`). Another
//!   holder of the Lease (seen once the API server answers again) makes it drop the
//!   address without writing.

use std::time::{Duration, Instant};

/// How long a pod waits per VIP it already holds before it takes a free one.
pub const SPREAD: Duration = Duration::from_millis(200);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
	pub duration: Duration,
	pub renew: Duration,
	pub retry: Duration,
}

impl Default for Timing {
	fn default() -> Self {
		Timing { duration: Duration::from_secs(3), renew: Duration::from_secs(1), retry: Duration::from_millis(500) }
	}
}

/// What to do when the API server cannot be reached (`--on-api-unreachable`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum OnApiUnreachable {
	/// Keep the VIP while rproxy is ready (another MAC announcing it makes the pod let go).
	Hold,
	/// Drop the VIP once the Lease would have expired.
	Release,
}

/// Why a VIP was let go (logs, `rproxy_vip_transitions_total{reason}`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
	Shutdown,
	NotReady,
	Cordoned,
	NotSelected,
	LeaseLost,
	Conflict,
	ApiUnreachable,
}

impl Reason {
	pub fn as_str(self) -> &'static str {
		match self {
			Reason::Shutdown => "shutdown",
			Reason::NotReady => "not_ready",
			Reason::Cordoned => "cordoned",
			Reason::NotSelected => "not_selected",
			Reason::LeaseLost => "lease_lost",
			Reason::Conflict => "conflict",
			Reason::ApiUnreachable => "api_unreachable",
		}
	}
}

/// The Lease as this pod last saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
	/// `holderIdentity` (empty: free).
	pub holder: String,
	/// `leaseDurationSeconds`.
	pub duration: Duration,
	/// When this pod first saw the Lease as it is now (its `resourceVersion`), or saw it
	/// again after the API server could not be reached.
	pub since: Instant,
}

/// What this pod knows about itself and the VIP.
#[derive(Clone, Debug, Default)]
pub struct Local {
	/// The address is on the interface (this pod put it there).
	pub holding: bool,
	/// The last successful write of the Lease.
	pub renewed: Option<Instant>,
	/// The last write tried.
	pub tried: Option<Instant>,
	/// Taken while the node was cordoned (because no other pod did): kept until a release for another reason.
	pub fallback: bool,
	/// No taking before this (after a conflict).
	pub backoff: Option<Instant>,
}

/// What is true now.
#[derive(Clone, Debug)]
pub struct Inputs<'a> {
	pub now: Instant,
	/// This pod's name in the Lease.
	pub me: &'a str,
	/// The Lease (`None`: not known yet, or missing).
	pub lease: Option<&'a View>,
	/// rproxy answers `/readyz` and the controller has applied the rule sets to this pod.
	pub ready: bool,
	/// The VIP may be on this node (its node selector matches, its interface is here).
	pub selected: bool,
	/// The node is cordoned (`spec.unschedulable`: a drain).
	pub cordoned: bool,
	pub shutdown: bool,
	/// Another MAC announced the VIP since the last decision.
	pub conflict: bool,
	/// How many other VIPs this pod holds.
	pub held_elsewhere: usize,
	/// The last time the API server could not be reached (a write or read failed).
	pub api_failed: Option<Instant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
	Wait,
	/// Write the Lease with this pod as holder, then add the address and announce it.
	Take,
	/// Write the Lease's renewal.
	Renew,
	/// Announce the address again (gratuitous ARP / unsolicited NA): another MAC said it has it.
	Announce,
	/// Remove the address, then clear the Lease's holder.
	Release(Reason),
	/// Remove the address without writing the Lease (another pod holds it, or it cannot be written).
	Drop(Reason),
}

fn elapsed(now: Instant, t: Option<Instant>) -> Option<Duration> {
	t.map(|t| now.saturating_duration_since(t))
}

/// What to do now.
pub fn decide(t: &Timing, on_unreachable: OnApiUnreachable, l: &Local, i: &Inputs) -> Action {
	if l.holding {
		if i.shutdown {
			return Action::Release(Reason::Shutdown);
		}
		if !i.ready {
			return Action::Release(Reason::NotReady);
		}
		if !i.selected {
			return Action::Release(Reason::NotSelected);
		}
		if i.cordoned && !l.fallback {
			return Action::Release(Reason::Cordoned);
		}
		if let Some(v) = i.lease {
			if !v.holder.is_empty() && v.holder != i.me {
				return Action::Drop(Reason::LeaseLost);
			}
		}
		let fresh = elapsed(i.now, l.renewed).is_some_and(|e| e < t.duration);
		if i.conflict {
			// a pod that renewed in time holds the Lease: the other MAC is stale (it answers again);
			// one that could not may have lost it to that MAC
			return if fresh { Action::Announce } else { Action::Release(Reason::Conflict) };
		}
		if !fresh && on_unreachable == OnApiUnreachable::Release && elapsed(i.now, l.renewed).is_some() {
			return Action::Drop(Reason::ApiUnreachable);
		}
		let due = elapsed(i.now, l.renewed).is_none_or(|e| e >= t.renew) && elapsed(i.now, l.tried).is_none_or(|e| e >= t.retry);
		return if due { Action::Renew } else { Action::Wait };
	}
	if i.shutdown || !i.ready || !i.selected || l.backoff.is_some_and(|b| i.now < b) {
		return Action::Wait;
	}
	let Some(v) = i.lease else { return Action::Wait };
	if !elapsed(i.now, l.tried).is_none_or(|e| e >= t.retry) {
		return Action::Wait;
	}
	if v.holder == i.me {
		// still ours (the sidecar restarted): take it back at once
		return Action::Take;
	}
	let free_at = if v.holder.is_empty() { v.since } else { v.since + v.duration };
	let mut wait = SPREAD * u32::try_from(i.held_elsewhere).unwrap_or(u32::MAX);
	if i.cordoned {
		wait += 2 * t.duration;
	}
	if !v.holder.is_empty() {
		// an expired holder: only once the API server has answered for a whole Lease duration
		// (after an outage the holder renews first, instead of everyone taking over at once)
		if elapsed(i.now, i.api_failed).is_some_and(|e| e < t.duration) {
			return Action::Wait;
		}
	}
	if i.now >= free_at + wait { Action::Take } else { Action::Wait }
}

#[cfg(test)]
mod tests {
	use super::*;

	struct Case {
		t0: Instant,
		t: Timing,
	}

	impl Case {
		fn new() -> Case {
			Case { t0: Instant::now(), t: Timing::default() }
		}
		fn at(&self, ms: u64) -> Instant {
			self.t0 + Duration::from_millis(ms)
		}
		fn view(&self, holder: &str, since_ms: u64) -> View {
			View { holder: holder.into(), duration: Duration::from_secs(3), since: self.at(since_ms) }
		}
		fn inputs<'a>(&self, now_ms: u64, lease: Option<&'a View>) -> Inputs<'a> {
			Inputs {
				now: self.at(now_ms),
				me: "pod-a",
				lease,
				ready: true,
				selected: true,
				cordoned: false,
				shutdown: false,
				conflict: false,
				held_elsewhere: 0,
				api_failed: None,
			}
		}
		fn holding(&self, renewed_ms: u64) -> Local {
			Local { holding: true, renewed: Some(self.at(renewed_ms)), tried: Some(self.at(renewed_ms)), ..Default::default() }
		}
		fn decide(&self, l: &Local, i: &Inputs) -> Action {
			decide(&self.t, OnApiUnreachable::Hold, l, i)
		}
	}

	#[test]
	fn takes_a_free_lease_when_ready() {
		let c = Case::new();
		let free = c.view("", 0);
		let idle = Local::default();
		assert_eq!(c.decide(&idle, &c.inputs(0, Some(&free))), Action::Take);
		assert_eq!(c.decide(&idle, &c.inputs(0, None)), Action::Wait, "the Lease is not known yet");
		let mut i = c.inputs(0, Some(&free));
		i.ready = false;
		assert_eq!(c.decide(&idle, &i), Action::Wait, "rproxy not ready or rule sets not applied");
		i.ready = true;
		i.selected = false;
		assert_eq!(c.decide(&idle, &i), Action::Wait, "not this node's VIP");
		i.selected = true;
		i.shutdown = true;
		assert_eq!(c.decide(&idle, &i), Action::Wait);
	}

	#[test]
	fn waits_for_the_holder_to_expire() {
		let c = Case::new();
		let held = c.view("pod-b", 1000);
		let idle = Local::default();
		assert_eq!(c.decide(&idle, &c.inputs(3999, Some(&held))), Action::Wait);
		assert_eq!(c.decide(&idle, &c.inputs(4000, Some(&held))), Action::Take, "3 s without a renewal seen");
		// the API server failed recently: the holder may renew first
		let mut i = c.inputs(4000, Some(&held));
		i.api_failed = Some(c.at(2000));
		assert_eq!(c.decide(&idle, &i), Action::Wait);
		i.now = c.at(5000);
		assert_eq!(c.decide(&idle, &i), Action::Take);
		// our own (the sidecar restarted): at once
		let ours = c.view("pod-a", 1000);
		assert_eq!(c.decide(&idle, &c.inputs(1000, Some(&ours))), Action::Take);
	}

	#[test]
	fn spreads_and_avoids_cordoned_nodes() {
		let c = Case::new();
		let free = c.view("", 0);
		let idle = Local::default();
		let mut i = c.inputs(399, Some(&free));
		i.held_elsewhere = 2;
		assert_eq!(c.decide(&idle, &i), Action::Wait, "200 ms per VIP held");
		i.now = c.at(400);
		assert_eq!(c.decide(&idle, &i), Action::Take);
		let mut i = c.inputs(5999, Some(&free));
		i.cordoned = true;
		assert_eq!(c.decide(&idle, &i), Action::Wait, "a cordoned node waits for others (2 x the duration)");
		i.now = c.at(6000);
		assert_eq!(c.decide(&idle, &i), Action::Take, "nobody else took it");
		// retried writes are spaced
		let tried = Local { tried: Some(c.at(5800)), ..Default::default() };
		assert_eq!(c.decide(&tried, &c.inputs(6000, Some(&free))), Action::Wait);
		assert_eq!(c.decide(&tried, &c.inputs(6300, Some(&free))), Action::Take);
	}

	#[test]
	fn renews_and_retries() {
		let c = Case::new();
		let ours = c.view("pod-a", 0);
		let l = c.holding(0);
		assert_eq!(c.decide(&l, &c.inputs(999, Some(&ours))), Action::Wait);
		assert_eq!(c.decide(&l, &c.inputs(1000, Some(&ours))), Action::Renew);
		// the renewal failed at 1000: again 500 ms later
		let failed = Local { tried: Some(c.at(1000)), ..c.holding(0) };
		assert_eq!(c.decide(&failed, &c.inputs(1400, Some(&ours))), Action::Wait);
		assert_eq!(c.decide(&failed, &c.inputs(1500, Some(&ours))), Action::Renew);
		// a cleared holder (by hand) is written again
		let free = c.view("", 0);
		assert_eq!(c.decide(&l, &c.inputs(1000, Some(&free))), Action::Renew);
	}

	#[test]
	fn releases_on_draining_shutdown_and_cordon() {
		let c = Case::new();
		let ours = c.view("pod-a", 0);
		let l = c.holding(0);
		let mut i = c.inputs(100, Some(&ours));
		i.ready = false;
		assert_eq!(c.decide(&l, &i), Action::Release(Reason::NotReady), "rproxy draining (/readyz 503)");
		let mut i = c.inputs(100, Some(&ours));
		i.shutdown = true;
		assert_eq!(c.decide(&l, &i), Action::Release(Reason::Shutdown));
		let mut i = c.inputs(100, Some(&ours));
		i.cordoned = true;
		assert_eq!(c.decide(&l, &i), Action::Release(Reason::Cordoned));
		let fallback = Local { fallback: true, ..c.holding(0) };
		assert_eq!(c.decide(&fallback, &i), Action::Wait, "taken on a cordoned node because nobody else did: kept");
		let mut i = c.inputs(100, Some(&ours));
		i.selected = false;
		assert_eq!(c.decide(&l, &i), Action::Release(Reason::NotSelected));
	}

	#[test]
	fn drops_a_lease_another_pod_holds() {
		let c = Case::new();
		let theirs = c.view("pod-b", 0);
		assert_eq!(c.decide(&c.holding(0), &c.inputs(100, Some(&theirs))), Action::Drop(Reason::LeaseLost));
	}

	#[test]
	fn api_server_unreachable() {
		let c = Case::new();
		let ours = c.view("pod-a", 0);
		// renewed last at 0, failing since
		let l = Local { tried: Some(c.at(9800)), ..c.holding(0) };
		// hold: kept while rproxy is ready, the renewal retried
		assert_eq!(c.decide(&l, &c.inputs(10_000, Some(&ours))), Action::Wait);
		assert_eq!(c.decide(&l, &c.inputs(10_300, Some(&ours))), Action::Renew);
		// release: dropped once the Lease would have expired
		assert_eq!(decide(&c.t, OnApiUnreachable::Release, &l, &c.inputs(2999, Some(&ours))), Action::Wait);
		assert_eq!(decide(&c.t, OnApiUnreachable::Release, &l, &c.inputs(3000, Some(&ours))), Action::Drop(Reason::ApiUnreachable));
	}

	#[test]
	fn conflicts() {
		let c = Case::new();
		let ours = c.view("pod-a", 0);
		// renewed in time: we hold the Lease; announce again
		let mut i = c.inputs(500, Some(&ours));
		i.conflict = true;
		assert_eq!(c.decide(&c.holding(0), &i), Action::Announce);
		// could not renew for the Lease duration (hold): another pod may have it
		i.now = c.at(3500);
		assert_eq!(c.decide(&c.holding(0), &i), Action::Release(Reason::Conflict));
		// not taken again before the backoff
		let after = Local { backoff: Some(c.at(7000)), ..Default::default() };
		let free = c.view("", 3600);
		assert_eq!(c.decide(&after, &c.inputs(6999, Some(&free))), Action::Wait);
		assert_eq!(c.decide(&after, &c.inputs(7000, Some(&free))), Action::Take);
	}

	#[test]
	fn reasons() {
		let all = [
			Reason::Shutdown,
			Reason::NotReady,
			Reason::Cordoned,
			Reason::NotSelected,
			Reason::LeaseLost,
			Reason::Conflict,
			Reason::ApiUnreachable,
		];
		let names: std::collections::BTreeSet<&str> = all.iter().map(|r| r.as_str()).collect();
		assert_eq!(names.len(), all.len());
	}
}
