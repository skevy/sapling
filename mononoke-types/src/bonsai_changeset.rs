// Copyright (c) 2018-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

use std::collections::BTreeMap;

use failure::{err_msg, SyncFailure};
use quickcheck::{Arbitrary, Gen};

use rust_thrift::compact_protocol;

use blob::{Blob, ChangesetBlob};
use datetime::DateTime;
use errors::*;
use file_change::FileChange;
use path::MPath;
use thrift;
use typed_hash::{ChangesetId, ChangesetIdContext};

/// A struct callers can use to build up a `BonsaiChangeset`.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct BonsaiChangesetMut {
    pub parents: Vec<ChangesetId>,
    pub author: String,
    pub author_date: DateTime,
    pub committer: Option<String>,
    // XXX should committer date always be recorded? If so, it should probably be a
    // monotonically increasing value:
    // max(author date, max(committer date of parents) + epsilon)
    pub committer_date: Option<DateTime>,
    pub message: String,
    pub extra: BTreeMap<String, String>,
    // XXX consider adding checks that:
    // * file_changes is ppf
    // * changeset IDs inside copy info in FileChange are all members of parents
    pub file_changes: BTreeMap<MPath, Option<FileChange>>,
}

impl BonsaiChangesetMut {
    /// Freeze this instance and turn it into a `BonsaiChangeset`.
    pub fn freeze(self) -> Result<BonsaiChangeset> {
        Ok(BonsaiChangeset { inner: self })
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct BonsaiChangeset {
    inner: BonsaiChangesetMut,
}

impl BonsaiChangeset {
    pub(crate) fn from_thrift(tc: thrift::BonsaiChangeset) -> Result<Self> {
        let catch_block = || {
            Ok(BonsaiChangesetMut {
                parents: tc.parents
                    .into_iter()
                    .map(|parent| ChangesetId::from_thrift(parent))
                    .collect::<Result<_>>()?,
                author: tc.author,
                author_date: DateTime::from_thrift(tc.author_date
                    .ok_or_else(|| err_msg("missing author date field"))?)?,
                committer: tc.committer,
                committer_date: match tc.committer_date {
                    Some(dt) => Some(DateTime::from_thrift(dt)?),
                    None => None,
                },
                message: tc.message,
                extra: tc.extra,
                file_changes: tc.file_changes
                    .into_iter()
                    .map(|(f, fc_opt)| {
                        let mpath = MPath::from_thrift(f)?;
                        let fc_opt = FileChange::from_thrift_opt(fc_opt, &mpath)?;
                        Ok((mpath, fc_opt))
                    })
                    .collect::<Result<_>>()?,
            }.freeze()?)
        };

        Ok(catch_block().with_context(|_: &Error| {
            ErrorKind::InvalidThrift("BonsaiChangeset".into(), "Invalid changeset".into())
        })?)
    }

    pub fn from_blob<T: AsRef<[u8]>>(t: T) -> Result<Self> {
        // TODO (T27336549) stop using SyncFailure once thrift is converted to failure
        let thrift_tc = compact_protocol::deserialize(t.as_ref())
            .map_err(SyncFailure::new)
            .context(ErrorKind::BlobDeserializeError("BonsaiChangeset".into()))?;
        Self::from_thrift(thrift_tc)
    }

    /// Get the parents for this changeset. The order of parents is significant.
    pub fn parents(&self) -> impl Iterator<Item = &ChangesetId> {
        self.inner.parents.iter()
    }

    /// Get the files changed in this changeset. The items returned are guaranteed
    /// to be in depth-first traversal order: once all the changes to a particular
    /// tree have been applied, it will never be referred to again.
    pub fn file_changes(&self) -> impl Iterator<Item = (&MPath, Option<&FileChange>)> {
        self.inner
            .file_changes
            .iter()
            .map(|(path, fc_opt)| (path, fc_opt.as_ref()))
    }

    /// Get the author for this changeset.
    pub fn author(&self) -> &str {
        &self.inner.author
    }

    /// Get the author date (time and timezone) for this changeset.
    pub fn author_date(&self) -> &DateTime {
        &self.inner.author_date
    }

    /// Get the committer for this changeset.
    pub fn committer(&self) -> Option<&str> {
        match self.inner.committer {
            Some(ref c) => Some(c.as_str()),
            None => None,
        }
    }

    /// Get the committer date (time and timezone) for this changeset.
    pub fn committer_date(&self) -> Option<&DateTime> {
        match self.inner.committer_date {
            Some(ref dt) => Some(dt),
            None => None,
        }
    }

    /// Get the commit message.
    pub fn message(&self) -> &str {
        &self.inner.message
    }

    /// Get the extra fields for this message.
    pub fn extra(&self) -> impl Iterator<Item = (&str, &str)> {
        self.inner
            .extra
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Allow mutating this instance of `BonsaiChangeset`.
    pub fn into_mut(self) -> BonsaiChangesetMut {
        self.inner
    }

    /// Serialize this structure into a blob.
    pub fn into_blob(self) -> ChangesetBlob {
        let thrift = self.into_thrift();
        let data = compact_protocol::serialize(&thrift);
        let mut context = ChangesetIdContext::new();
        context.update(&data);
        let id = context.finish();
        Blob::new(id, data)
    }

    pub(crate) fn into_thrift(self) -> thrift::BonsaiChangeset {
        thrift::BonsaiChangeset {
            parents: self.inner
                .parents
                .into_iter()
                .map(|parent| parent.into_thrift())
                .collect(),
            author: self.inner.author,
            author_date: Some(self.inner.author_date.into_thrift()),
            committer: self.inner.committer,
            committer_date: self.inner.committer_date.map(|dt| dt.into_thrift()),
            message: self.inner.message,
            extra: self.inner.extra,
            file_changes: self.inner
                .file_changes
                .into_iter()
                .map(|(f, c)| (f.into_thrift(), FileChange::into_thrift_opt(c)))
                .collect(),
        }
    }
}

impl Arbitrary for BonsaiChangeset {
    fn arbitrary<G: Gen>(g: &mut G) -> Self {
        // In the future Mononoke would like to support changesets with more parents than 2.
        // Start testing that now.
        let num_parents = g.gen_range(0, 8);
        let parents = (0..num_parents)
            .map(|_| ChangesetId::arbitrary(g))
            .collect();
        BonsaiChangesetMut {
            parents,
            file_changes: BTreeMap::arbitrary(g),
            author: String::arbitrary(g),
            author_date: DateTime::arbitrary(g),
            committer: Option::<String>::arbitrary(g),
            committer_date: Option::<DateTime>::arbitrary(g),
            message: String::arbitrary(g),
            extra: BTreeMap::arbitrary(g),
        }.freeze()
            .expect("generated bonsai changeset must be valid")
    }

    fn shrink(&self) -> Box<Iterator<Item = Self>> {
        let cs = self.clone().inner;
        let iter = (
            cs.parents.clone(),
            cs.file_changes.clone(),
            cs.extra.clone(),
        ).shrink()
            .map(move |(parents, file_changes, extra)| {
                BonsaiChangesetMut {
                    parents,
                    file_changes,
                    author: cs.author.clone(),
                    author_date: cs.author_date,
                    committer: cs.committer.clone(),
                    committer_date: cs.committer_date,
                    message: cs.message.clone(),
                    extra,
                }.freeze()
                    .expect("shrunken bonsai changeset must be valid")
            });
        Box::new(iter)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use std::str::FromStr;

    use file_change::FileType;
    use hash::Blake2;
    use typed_hash::ContentId;

    quickcheck! {
        fn thrift_roundtrip(cs: BonsaiChangeset) -> bool {
            let thrift_cs = cs.clone().into_thrift();
            let cs2 = BonsaiChangeset::from_thrift(thrift_cs)
                .expect("thrift roundtrips should always be valid");
            cs == cs2
        }

        fn blob_roundtrip(cs: BonsaiChangeset) -> bool {
            let blob = cs.clone().into_blob();
            let cs2 = BonsaiChangeset::from_blob(blob.data().as_ref())
                .expect("blob roundtrips should always be valid");
            cs == cs2
        }
    }

    #[test]
    fn fixed_blob() {
        let tc = BonsaiChangesetMut {
            parents: vec![],
            author: "foo".into(),
            author_date: DateTime::from_timestamp(1234567890, 36800).unwrap(),
            committer: Some("bar".into()),
            committer_date: Some(DateTime::from_timestamp(1500000000, -36800).unwrap()),
            message: "Commit message".into(),
            extra: BTreeMap::new(),
            file_changes: btreemap![
                MPath::new("a/b").unwrap() => Some(FileChange::new(
                    ContentId::from_byte_array([1; 32]),
                    FileType::Regular,
                    42,
                    None,
                )),
                MPath::new("c/d").unwrap() => Some(FileChange::new(
                    ContentId::from_byte_array([2; 32]),
                    FileType::Executable,
                    84,
                    Some((
                        MPath::new("e/f").unwrap(),
                        ChangesetId::from_byte_array([3; 32]),
                    )),
                )),
                MPath::new("g/h").unwrap() => None,
                MPath::new("i/j").unwrap() => None,
            ],
        };
        let tc = tc.freeze().expect("fixed bonsai changeset must be valid");
        let blob = tc.into_blob();

        assert_eq!(
            blob.id(),
            &ChangesetId::new(
                Blake2::from_str(
                    "dfb3d7163d601880458752efcaf158e66178a2f29223b2a918a697faaeee8159"
                ).unwrap()
            )
        );
    }
}
