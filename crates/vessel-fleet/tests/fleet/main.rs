//! `vessel-fleet` over temporary trees: bundling a home's sources, moving
//! blobs to a member, and materialising them into harness homes with
//! local edits kept, conflicts listed and symlinks never followed.

#[cfg(test)]
mod blobs;
#[cfg(test)]
#[cfg(unix)]
mod home;
#[cfg(test)]
mod member;
#[cfg(test)]
mod names;
#[cfg(test)]
mod safety;
#[cfg(test)]
mod support;
#[cfg(test)]
mod transfers;
