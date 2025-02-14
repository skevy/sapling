/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include "eden/fs/utils/ImmediateFuture.h"

namespace facebook::eden {

/**
 * A connection to a userspace filesystem driver.
 *
 * In practice, this is FuseChannel, Nfsd3, or PrjfsChannel.
 */
class FsChannel {
 public:
  virtual ~FsChannel() = default;

  /**
   * During checkout or other Thrift calls that modify the filesystem, those
   * modifications may be invisible to the filesystem's own caches. Therefore,
   * we send fine-grained invalidation messages to the FsChannel. Those
   * invalidations may be asynchronous, but we need to ensure that they have
   * been observed by the time the Thrift call completes.
   *
   * You may think of completeInvalidations() as a fence; after
   * completeInvalidations() completes, invalidations of inode attributes, inode
   * content, and name lookups are guaranteed to be observable.
   */
  FOLLY_NODISCARD virtual ImmediateFuture<folly::Unit>
  completeInvalidations() = 0;
};

} // namespace facebook::eden
