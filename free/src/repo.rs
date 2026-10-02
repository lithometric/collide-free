//! Repo identity and scope keying.
//!
//! Every piece of a repo's state lives under `{workspace_id}:{repo_id}`, and
//! that key is never re-derived once written. So the question this module
//! answers — "which canonical id does this incoming name mean?" — decides
//! whether an agent finds its teammates' work or a fresh empty scope.
//!
//! The rules are deliberately conservative. A friendly short name and the
//! full git URL of the same repo share one scope; two genuinely different
//! repos that merely share a basename never do. When it cannot tell, it
//! demands an exact match rather than guessing.

use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

use sha1::{Digest, Sha1};

/// The ephemeral-key form of a path: short, fixed width, and not the path
/// itself (these keys end up in scan prefixes).
///
/// sha1, not sha256 — and that is not an aesthetic choice. Both servers scan
/// `hot:{scope}:{path_key}:` and write `salience:{scope}:{path_key}`, so a
/// different digest here would silently split every path into two key spaces:
/// the Rust gate would find no hot markers, the Python dashboard would show
/// half the scar tissue, and nothing would error. The parity test pins it.
pub fn path_key(path: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(path.as_bytes());
    format!("{:x}", hasher.finalize())[..16].to_string()
}

/// A friendly key so equivalent forms of the same repo collide: the bare name
/// and any host-qualified URL share one. `github.com/acme/app`,
/// `git@github.com:acme/app.git`, a local path (`C:\src\app` too) and `app`
/// all normalize to `app`; a
/// branch-qualified `app#main` keeps its suffix and is a different scope.
///
/// Advisory only — two same-named repos in different orgs are separated by
/// the alias registry, never silently merged.
pub fn normalize_repo_id(repo_id: &str) -> String {
    let mut s = repo_id.trim().to_lowercase();
    if s.is_empty() {
        return s;
    }
    for scheme in ["https://", "http://", "ssh://", "git://"] {
        if let Some(rest) = s.strip_prefix(scheme) {
            s = rest.to_string();
            break;
        }
    }
    if let Some(rest) = s.strip_prefix("git@") {
        s = rest.to_string();
    }
    // a Windows path (an agent passing its working directory, C:\Users\me\app)
    // splits like a POSIX one, so it lands on the repo's name, not a phantom scope
    s = s.replace([':', '\\'], "/");
    let s = s.trim_end_matches('/');
    let s = s.strip_suffix(".git").unwrap_or(s);
    let base = s.rsplit('/').next().unwrap_or(s);
    if base.is_empty() { s.to_string() } else { base.to_string() }
}

/// A folder on one machine rather than a repository: a `local/...` id (what
/// the free version names a repo with no remote), an absolute path (an agent
/// passing its working directory), a home-relative one. No one else could be
/// working in it, so it never joins a workspace. Repo names and `owner/name`
/// stay welcome: they resolve onto the repo they name.
pub fn is_machine_folder(repo_id: &str) -> bool {
    let id = repo_id.trim();
    let bytes = id.as_bytes();
    id.starts_with("local/")
        || id.starts_with('/')
        || id.starts_with('~')
        || id.starts_with('\\')
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
}

/// The access-list key for a repo id: lowercase, scheme and `.git` stripped,
/// no `#branch` — so `github.com/Acme/API#main` and `acme/api` match.
pub fn repo_key(repo_id: &str) -> String {
    let mut s = repo_id.trim().to_lowercase();
    for scheme in ["https://", "http://", "ssh://", "git://"] {
        if let Some(rest) = s.strip_prefix(scheme) {
            s = rest.to_string();
            break;
        }
    }
    if let Some(rest) = s.strip_prefix("git@") {
        s = rest.replacen(':', "/", 1);
    }
    let s = s.split('#').next().unwrap_or("").trim_end_matches('/');
    s.strip_suffix(".git").unwrap_or(s).to_string()
}

/// The org/owner segment of a host-qualified id; empty for bare names.
/// Used by workspace routing, which has not moved across yet.
#[allow(dead_code)]
pub fn repo_org(repo_id: &str) -> String {
    let key = repo_key(repo_id);
    let parts: Vec<&str> = key.split('/').collect();
    if parts.len() >= 2 { parts[parts.len() - 2].to_string() } else { String::new() }
}

/// How host/org-qualified an id is; a full URL outranks a bare name when both
/// claim the same normalized key.
fn qualification(repo_id: &str) -> usize {
    repo_id.matches('/').count()
}

/// First id seen for a normalized key wins; genuinely ambiguous keys fall
/// back to exact matching.
#[derive(Default)]
pub struct Aliases {
    alias: RwLock<HashMap<String, String>>,
    ambiguous: RwLock<HashSet<String>>,
    renames: RwLock<HashMap<String, String>>,
}

impl Aliases {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a durable rename: a repo renamed on GitHub reports under its
    /// new id while its history lives under the old one.
    pub fn add_rename(&self, workspace_id: &str, from: &str, canonical: &str) {
        if let Ok(mut renames) = self.renames.write() {
            renames.insert(format!("{workspace_id}:{from}"), canonical.to_string());
        }
    }

    /// Seed from scopes that already carry data, so a friendly alias resolves
    /// to the existing canonical scope instead of minting an empty one. A
    /// fuller URL wins over a bare name; a key two equally-qualified repos
    /// both claim is marked ambiguous and demands an exact id thereafter.
    pub fn seed(&self, scopes: &[String]) {
        let mut best: HashMap<String, String> = HashMap::new();
        let mut ambiguous: HashSet<String> = HashSet::new();
        for scope in scopes {
            let Some((workspace_id, repo_id)) = scope.split_once(':') else { continue };
            if repo_id.is_empty() {
                continue;
            }
            let nkey = format!("{workspace_id}:{}", normalize_repo_id(repo_id));
            if ambiguous.contains(&nkey) {
                continue;
            }
            match best.get(&nkey) {
                None => {
                    best.insert(nkey, repo_id.to_string());
                }
                Some(current) if current == repo_id => {}
                Some(current) => {
                    let (new_rank, old_rank) = (qualification(repo_id), qualification(current));
                    if new_rank > old_rank {
                        best.insert(nkey, repo_id.to_string());
                    } else if new_rank == old_rank {
                        // two distinct repos, same basename, equally qualified:
                        // aliasing either way would merge them
                        ambiguous.insert(nkey.clone());
                        best.remove(&nkey);
                    }
                }
            }
        }
        if let Ok(mut alias) = self.alias.write() {
            alias.extend(best);
        }
        if let Ok(mut marked) = self.ambiguous.write() {
            marked.extend(ambiguous);
        }
    }

    /// Force `{workspace_id}:normalize(repo_id)` to resolve onto `canonical`,
    /// overwriting any existing claim. `resolve`'s first-seen rule is right
    /// for organic discovery, but a repo that just landed in a workspace
    /// (`move_repo`) needs its bare basename bridged to it immediately —
    /// waiting for the next `resolve` call could instead confirm a stale
    /// claim left by whatever used to sit at that key.
    pub fn force_alias(&self, workspace_id: &str, repo_id: &str, canonical: &str) {
        let nkey = format!("{workspace_id}:{}", normalize_repo_id(repo_id));
        if let Ok(mut alias) = self.alias.write() {
            alias.insert(nkey, canonical.to_string());
        }
    }

    /// Map an incoming repo id to the canonical id already holding this
    /// workspace's data.
    pub fn resolve(&self, workspace_id: &str, repo_id: &str) -> String {
        self.resolve_inner(workspace_id, repo_id, 0)
    }

    fn resolve_inner(&self, workspace_id: &str, repo_id: &str, depth: usize) -> String {
        if depth > 4 {
            return repo_id.to_string();
        }
        // an explicit rename comes first: redirect before basename aliasing so
        // the new name lands on the old scope
        let mut repo_id = repo_id.to_string();
        if let Ok(renames) = self.renames.read() {
            if let Some(canonical) = renames.get(&format!("{workspace_id}:{repo_id}")) {
                if canonical != &repo_id {
                    repo_id = canonical.clone();
                }
            }
        }

        // a #branch-suffixed id whose base this workspace already knows is the
        // SAME repo on a branch, not a second repo: fold it onto the base
        // scope (the branch rides every event as metadata instead). A
        // suffixed id with no known base keeps its own scope.
        if let Some((base, _branch)) = repo_id.split_once('#') {
            let base = base.trim();
            if !base.is_empty() {
                let base_key = format!("{workspace_id}:{}", normalize_repo_id(base));
                let known = self.alias.read().map(|a| a.contains_key(&base_key)).unwrap_or(false);
                if known {
                    return self.resolve_inner(workspace_id, base, depth + 1);
                }
            }
        }

        let nkey = format!("{workspace_id}:{}", normalize_repo_id(&repo_id));
        if self.ambiguous.read().map(|a| a.contains(&nkey)).unwrap_or(false) {
            return repo_id;
        }
        let existing = self.alias.read().ok().and_then(|a| a.get(&nkey).cloned());
        if let Some(canonical) = existing {
            // basename aliasing bridges a BARE name and the full URL of one
            // repo — never two host-qualified ids that merely share a basename
            // (github.com/acme/api and github.com/other/api are two repos)
            if canonical != repo_id
                && canonical.contains('/')
                && repo_id.contains('/')
                && repo_key(&canonical) != repo_key(&repo_id)
            {
                return repo_id;
            }
            return canonical;
        }
        if let Ok(mut alias) = self.alias.write() {
            alias.insert(nkey, repo_id.clone()); // first-seen claims the key
        }
        repo_id
    }

    pub fn scope_for(&self, workspace_id: &str, repo_id: &str) -> String {
        format!("{workspace_id}:{}", self.resolve(workspace_id, repo_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_on_one_machine_never_joins_a_workspace() {
        for folder in ["local/my-app", r"C:\Users\jo\app", "C:/Users/jo/app", "/home/jo/app", "~/code/app", r"\\server\share\app"] {
            assert!(is_machine_folder(folder), "{folder}");
        }
        for repo in ["github.com/acme/api", "git@gitlab.com:group/app.git", "acme/api", "collide", "r-doc-empty"] {
            assert!(!is_machine_folder(repo), "{repo}");
        }
    }

    #[test]
    fn a_working_directory_lands_on_the_repo_name_on_every_platform() {
        for id in ["github.com/acme/app", "git@github.com:acme/app.git", r"C:\Users\jo\app", r"C:\Users\jo\app\", "/home/jo/app", "App"] {
            assert_eq!(normalize_repo_id(id), "app", "{id}");
        }
    }
}
