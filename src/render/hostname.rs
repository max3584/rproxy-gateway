//! Gateway API host names: `example.com` or `*.example.com`, where the wildcard
//! stands for one or more labels (`a.example.com`, `a.b.example.com`, not
//! `example.com` itself). rproxy writes the same wildcard as `**.example.com`
//! (`*.` there is exactly one label).

/// Whether the host name pattern `pattern` matches the name `name` (no wildcard in `name`).
pub fn matches(pattern: &str, name: &str) -> bool {
	let (pattern, name) = (pattern.to_ascii_lowercase(), name.to_ascii_lowercase());
	match pattern.strip_prefix("*.") {
		Some(suffix) => name.len() > suffix.len() + 1 && name.ends_with(&format!(".{suffix}")),
		None => pattern == name,
	}
}

/// Whether every name `inner` matches is also matched by `outer`.
pub fn covers(outer: &str, inner: &str) -> bool {
	match (outer.strip_prefix("*."), inner.strip_prefix("*.")) {
		(Some(_), Some(inner_suffix)) => outer.eq_ignore_ascii_case(inner) || matches(outer, inner_suffix),
		(Some(_), None) => matches(outer, inner),
		(None, Some(_)) => false,
		(None, None) => outer.eq_ignore_ascii_case(inner),
	}
}

/// The names both patterns match, as one pattern (the narrower one), if any.
pub fn intersect(a: &str, b: &str) -> Option<String> {
	if covers(a, b) {
		Some(b.to_ascii_lowercase())
	} else if covers(b, a) {
		Some(a.to_ascii_lowercase())
	} else {
		None
	}
}

/// No host name in common: the route does not attach to the listener.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoMatch;

/// The host names a route serves on a listener (Gateway API rules): `Ok(None)` is
/// any host, `Ok(Some(names))` those names, `Err(NoMatch)` none.
pub fn effective(listener: Option<&str>, route: &[String]) -> Result<Option<Vec<String>>, NoMatch> {
	match (listener, route.is_empty()) {
		(None, true) => Ok(None),
		(None, false) => Ok(Some(dedup(route.iter().map(|h| h.to_ascii_lowercase()).collect()))),
		(Some(l), true) => Ok(Some(vec![l.to_ascii_lowercase()])),
		(Some(l), false) => {
			let names: Vec<String> = route.iter().filter_map(|h| intersect(l, h)).collect();
			if names.is_empty() { Err(NoMatch) } else { Ok(Some(dedup(names))) }
		}
	}
}

fn dedup(mut names: Vec<String>) -> Vec<String> {
	let mut seen = std::collections::HashSet::new();
	names.retain(|n| seen.insert(n.clone()));
	names
}

/// The pattern in rproxy's syntax (`Host(...)`, `tls.routes[].server_names`).
pub fn to_rproxy(pattern: &str) -> String {
	match pattern.strip_prefix("*.") {
		Some(suffix) => format!("**.{suffix}"),
		None => pattern.to_string(),
	}
}

/// How specific a host name is, for ordering routes (more specific first):
/// exact names before wildcards, longer before shorter; `None` (any host) last.
pub fn specificity(name: Option<&str>) -> (u8, usize) {
	match name {
		None => (0, 0),
		Some(n) if n.starts_with("*.") => (1, n.len()),
		Some(n) => (2, n.len()),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn wildcards_match_one_or_more_labels() {
		assert!(matches("*.example.com", "a.example.com"));
		assert!(matches("*.example.com", "a.b.example.com"));
		assert!(!matches("*.example.com", "example.com"));
		assert!(matches("Example.COM", "example.com"));
		assert!(!matches("example.com", "a.example.com"));
	}

	#[test]
	fn intersections() {
		assert_eq!(intersect("*.example.com", "foo.example.com").as_deref(), Some("foo.example.com"));
		assert_eq!(intersect("foo.example.com", "*.example.com").as_deref(), Some("foo.example.com"));
		assert_eq!(intersect("*.example.com", "*.foo.example.com").as_deref(), Some("*.foo.example.com"));
		assert_eq!(intersect("*.example.com", "example.com"), None);
		assert_eq!(intersect("a.example.com", "b.example.com"), None);
		assert_eq!(effective(None, &[]), Ok(None));
		assert_eq!(effective(Some("*.example.com"), &[]), Ok(Some(vec!["*.example.com".into()])));
		assert_eq!(
			effective(Some("*.example.com"), &["a.example.com".into(), "other.net".into(), "*.example.com".into()]),
			Ok(Some(vec!["a.example.com".into(), "*.example.com".into()]))
		);
		assert_eq!(effective(Some("a.example.com"), &["b.example.com".into()]), Err(NoMatch));
	}

	#[test]
	fn rproxy_syntax_and_order() {
		assert_eq!(to_rproxy("*.example.com"), "**.example.com");
		assert_eq!(to_rproxy("a.example.com"), "a.example.com");
		assert!(specificity(Some("a.example.com")) > specificity(Some("*.example.com")));
		assert!(specificity(Some("*.a.example.com")) > specificity(Some("*.example.com")));
		assert!(specificity(Some("*.example.com")) > specificity(None));
	}
}
