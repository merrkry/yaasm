# YAASM

YAASM manages a user's declared collection of agent skills and exposes them through one or more tool-specific directories.

## Language

**Configuration**:
The user-level declaration of desired sources, skills, and targets.
_Avoid_: Project configuration, manifest

**Data directory**:
The YAASM-owned root from which deployed skill content is exposed for one invocation profile. Only links into the current data directory are managed.
_Avoid_: Configuration directory, target

**Source**:
A named origin from which one or more skills can be obtained. A source may be a Git repository, GitHub repository, or local directory.
_Avoid_: Registry, package

**Network source**:
A Git or GitHub source whose resolved version is recorded in the lockfile.
_Avoid_: Remote package

**Deployable revision**:
A resolved network source revision that, when updated, contained every configured skill path with a valid `SKILL.md` and no unsupported Git submodule. Deployment validates the locked contents again against the current configuration.
_Avoid_: Valid commit, latest version

**Local source**:
A reserved source named `local` that selects a directory on the user's machine. It remains live after deployment and is not recorded in the lockfile.
_Avoid_: Development source, unlocked source

**Skill**:
A named directory selected from a source for deployment to targets. Its configured name is both its target link name and its name inside YAASM's data directory.
_Avoid_: Package, plugin

**Snapshot**:
A logically immutable partial source tree containing the configured skill paths at a network source's locked revision. Its identity covers the source, revision, and selected paths; each skill data entry points to its directory within the shared tree.
_Avoid_: Checkout, installation

**Bundle**:
A portable, caller-owned collection of selected network skill contents and their locked identities. It excludes local skills and targets and supplies network sources for offline deployment.
_Avoid_: Deployment, snapshot

**Target**:
A user directory in which declared skills are made available to an agent tool.
_Avoid_: Install directory, destination

**Placement**:
The deployment of one skill into one target. It is the leaf of the source-to-skill-to-placement dependency hierarchy.
_Avoid_: Target link, installation

**Deployment map**:
The set of configured placements confirmed successful for a target during the current deployment. It is the authority for cleaning that target's stale managed links.
_Avoid_: State file, link registry

**Managed link**:
A link in a configured target whose immediate destination is anywhere inside YAASM's current data directory. YAASM owns managed links but not other entries in a target.
_Avoid_: Installed skill, managed file

**Lockfile**:
The generated mapping from a network source's configuration fingerprint to its resolved revision. Every declared network source must have an entry before it can be deployed. The mapping is created when absent and automatically reconciled with configuration changes.
_Avoid_: Configuration, manifest

**Update**:
The resolution, validation, and materialization of a network source followed by committing its deployable revision to the lockfile. Explicit updates and updates needed to reconcile missing or stale lock entries use the same process.
_Avoid_: Deployment, installation

**Deployment**:
The reconciliation of skill data entries and target placements with the configuration and lockfile. The `deploy` command performs it after source-resolution attempts and cannot roll back successful updates. The `update` command prepares sources without deployment.
_Avoid_: Update, installation
