#!/bin/sh
#
# This script creates a Git repo in the current directory.
#
# The repo contains several crates to test the `CrateGraph` code against a
# somewhat realistic multi-workspace repo.
#
# The repo also has multiple revisions to test functions that do change
# detection against a PR branch.

set -eo pipefail

TEST_DATA=$1
REV1_CONTENT=$TEST_DATA/rev1_content
REV2_CONTENT=$TEST_DATA/rev2_content

git init

# 1ST COMMIT: Initialize crates and create dependencies

# Create crates.

# Standalone package.
cargo init --lib standalone

# Workspace with root package and one member.
cargo init --lib foo
cargo init --lib foo/foo_member1

# Standalone package with nested non-member crate.
cargo init --lib bar
cargo init --lib bar/bar_nested

# Workspace with only one member.
cargo init --lib baz
cargo init --lib baz/baz_member1

# Crate with sentinel file that causes it to be ignored.
cargo init --lib skipped
touch skipped/.skip_ci

# Create some junk directories that should be ignored.
mkdir red_herring
touch red_herring/junk
mkdir bar/red_herring
touch bar/red_herring/junk

# Overwrite Cargo.toml files to create dependencies between crates.
cp -r $REV1_CONTENT/* .

git add .
git commit -am "initialize crates"

# 2ND COMMIT: Touch a subset of crates

cp -r $REV2_CONTENT/* .

git commit -am "edit foo and baz_member1"
