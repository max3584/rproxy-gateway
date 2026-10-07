//! Leader election with a Lease (coordination.k8s.io/v1), so that several
//! controller replicas can run and only one renders, PUTs rule sets and writes
//! status at a time.
//!
//! - The leader renews the Lease every `renew` (default 5 s); the Lease is held
//!   for `duration` (default 15 s) after the last renewal.
//! - A replica takes the Lease when it has no holder or its holder has not
//!   renewed it within `duration`. Writes use the Lease's `resourceVersion`, so
//!   two replicas cannot both take it.
//! - The leader steps down when it could not renew for `deadline` (default
//!   10 s, shorter than `duration`): it stops before another replica can take
//!   over. On shutdown it gives the Lease up so another replica takes over at once.

use std::time::{Duration, Instant};

use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta};
use kube::Api;
use kube::api::PostParams;
use tokio::sync::watch;
use tracing::{info, warn};

#[derive(Clone, Debug)]
pub struct Settings {
	/// The Lease's name (in the controller's namespace).
	pub lease: String,
	/// This replica's name in the Lease (`holderIdentity`): the pod name.
	pub identity: String,
	pub duration: Duration,
	pub renew: Duration,
	pub deadline: Duration,
}

impl Settings {
	pub fn new(lease: &str, identity: &str) -> Settings {
		Settings {
			lease: lease.into(),
			identity: identity.into(),
			duration: Duration::from_secs(15),
			renew: Duration::from_secs(5),
			deadline: Duration::from_secs(10),
		}
	}
}

/// What to do with the Lease as it is now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
	/// No Lease yet: create it, held by us.
	Create,
	/// We hold it: renew it.
	Renew,
	/// Free or expired: take it.
	Take,
	/// Another replica holds it.
	Wait { holder: String },
}

/// Decides from the Lease (`None`: it does not exist) at `now`.
pub fn step(lease: Option<&Lease>, identity: &str, now: jiff::Timestamp) -> Step {
	let Some(lease) = lease else { return Step::Create };
	let spec = lease.spec.clone().unwrap_or_default();
	let holder = spec.holder_identity.unwrap_or_default();
	if holder == identity {
		return Step::Renew;
	}
	if holder.is_empty() {
		return Step::Take;
	}
	let duration = jiff::SignedDuration::from_secs(i64::from(spec.lease_duration_seconds.unwrap_or(15).max(1)));
	match spec.renew_time.or(spec.acquire_time) {
		Some(t) if t.0.saturating_add(duration).is_ok_and(|until| until > now) => Step::Wait { holder },
		_ => Step::Take,
	}
}

fn spec(identity: &str, s: &Settings, now: jiff::Timestamp, transitions: i32) -> LeaseSpec {
	LeaseSpec {
		holder_identity: Some(identity.into()),
		lease_duration_seconds: Some(s.duration.as_secs().max(1) as i32),
		acquire_time: Some(MicroTime(now)),
		renew_time: Some(MicroTime(now)),
		lease_transitions: Some(transitions),
		..Default::default()
	}
}

/// One attempt: whether we hold the Lease afterwards.
pub async fn try_hold(api: &Api<Lease>, s: &Settings) -> anyhow::Result<bool> {
	let now = jiff::Timestamp::now();
	let current = api.get_opt(&s.lease).await?;
	let pp = PostParams::default();
	let result = match step(current.as_ref(), &s.identity, now) {
		Step::Wait { .. } => return Ok(false),
		Step::Create => {
			let lease = Lease {
				metadata: ObjectMeta { name: Some(s.lease.clone()), ..Default::default() },
				spec: Some(spec(&s.identity, s, now, 0)),
			};
			api.create(&pp, &lease).await.map(|_| ())
		}
		Step::Renew => {
			let mut lease = current.expect("renewing an existing Lease");
			let sp = lease.spec.get_or_insert_default();
			sp.renew_time = Some(MicroTime(now));
			sp.lease_duration_seconds = Some(s.duration.as_secs().max(1) as i32);
			api.replace(&s.lease, &pp, &lease).await.map(|_| ())
		}
		Step::Take => {
			let mut lease = current.expect("taking an existing Lease");
			let transitions = lease.spec.as_ref().and_then(|sp| sp.lease_transitions).unwrap_or(0).saturating_add(1);
			// metadata (resourceVersion) is kept: the write fails if another replica wrote first
			lease.spec = Some(spec(&s.identity, s, now, transitions));
			api.replace(&s.lease, &pp, &lease).await.map(|_| ())
		}
	};
	match result {
		Ok(()) => Ok(true),
		// another replica created or wrote it first
		Err(kube::Error::Api(e)) if e.code == 409 => Ok(false),
		Err(e) => Err(e.into()),
	}
}

/// Gives the Lease up if we hold it (on shutdown).
pub async fn release(api: &Api<Lease>, s: &Settings) {
	let Ok(Some(mut lease)) = api.get_opt(&s.lease).await else { return };
	if lease.spec.as_ref().and_then(|sp| sp.holder_identity.as_deref()) != Some(s.identity.as_str()) {
		return;
	}
	let sp = lease.spec.get_or_insert_default();
	sp.holder_identity = None;
	// expired at once for whoever reads it next
	sp.lease_duration_seconds = Some(1);
	match api.replace(&s.lease, &PostParams::default(), &lease).await {
		Ok(_) => info!(lease = s.lease, "leadership released"),
		Err(e) => warn!(lease = s.lease, error = %e, "cannot release the Lease"),
	}
}

/// Keeps trying to hold the Lease; `tx` says whether we are the leader now.
pub async fn run(api: Api<Lease>, s: Settings, tx: watch::Sender<bool>) {
	let mut last_held: Option<Instant> = None;
	loop {
		let leading = *tx.borrow();
		match try_hold(&api, &s).await {
			Ok(true) => {
				last_held = Some(Instant::now());
				if !leading {
					info!(lease = s.lease, identity = s.identity, "became the leader");
					let _ = tx.send(true);
				}
			}
			Ok(false) => {
				if leading {
					warn!(lease = s.lease, "lost the Lease to another replica");
					let _ = tx.send(false);
				}
				last_held = None;
			}
			Err(e) => {
				warn!(lease = s.lease, error = %e, "cannot read or write the Lease");
				if leading && last_held.is_none_or(|t| t.elapsed() >= s.deadline) {
					warn!(lease = s.lease, "could not renew the Lease in time; stepping down");
					let _ = tx.send(false);
					last_held = None;
				}
			}
		}
		let wait = if *tx.borrow() { s.renew } else { s.renew.min(Duration::from_secs(2)) };
		tokio::time::sleep(wait).await;
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn lease(holder: Option<&str>, renewed: jiff::Timestamp, secs: i32) -> Lease {
		Lease {
			metadata: ObjectMeta::default(),
			spec: Some(LeaseSpec {
				holder_identity: holder.map(str::to_string),
				lease_duration_seconds: Some(secs),
				renew_time: Some(MicroTime(renewed)),
				..Default::default()
			}),
		}
	}

	#[test]
	fn steps() {
		let now = jiff::Timestamp::from_second(1_000_000).unwrap();
		let ago = |s: i64| now.checked_sub(jiff::SignedDuration::from_secs(s)).unwrap();
		assert_eq!(step(None, "a", now), Step::Create);
		assert_eq!(step(Some(&lease(Some("a"), ago(100), 15)), "a", now), Step::Renew);
		assert_eq!(step(Some(&lease(Some("b"), ago(5), 15)), "a", now), Step::Wait { holder: "b".into() });
		assert_eq!(step(Some(&lease(Some("b"), ago(16), 15)), "a", now), Step::Take);
		assert_eq!(step(Some(&lease(None, ago(1), 15)), "a", now), Step::Take);
		let mut never = lease(Some("b"), now, 15);
		never.spec.as_mut().unwrap().renew_time = None;
		assert_eq!(step(Some(&never), "a", now), Step::Take);
	}
}
