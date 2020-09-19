/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/fuse/privhelper/PrivHelperImpl.h"

#include <folly/Exception.h>
#include <folly/Expected.h>
#include <folly/File.h>
#include <folly/FileUtil.h>
#include <folly/String.h>
#include <folly/Synchronized.h>
#include <folly/futures/Future.h>
#include <folly/init/Init.h>
#include <folly/io/Cursor.h>
#include <folly/io/async/EventBase.h>
#include <folly/logging/Init.h>
#include <folly/logging/xlog.h>
#include <folly/portability/SysTypes.h>
#include <folly/portability/Unistd.h>
#ifndef _WIN32
#include <sys/wait.h>
#endif // !_WIN32

#include "eden/fs/fuse/privhelper/PrivHelper.h"

#ifndef _WIN32
#include "eden/fs/fuse/privhelper/PrivHelperConn.h"
#include "eden/fs/fuse/privhelper/PrivHelperServer.h"
#include "eden/fs/utils/Bug.h"
#include "eden/fs/utils/FileDescriptor.h"
#include "eden/fs/utils/PathFuncs.h"
#include "eden/fs/utils/SpawnedProcess.h"
#include "eden/fs/utils/UserInfo.h"
#endif // _WIN32

using folly::checkUnixError;
using folly::EventBase;
using folly::File;
using folly::Future;
using folly::StringPiece;
using folly::Unit;
using folly::io::Cursor;
using std::make_unique;
using std::string;
using std::unique_ptr;
using std::vector;

namespace facebook {
namespace eden {

#ifndef _WIN32

namespace {

/**
 * PrivHelperClientImpl contains the client-side logic (in the parent process)
 * for talking to the remote privileged process.
 */
class PrivHelperClientImpl : public PrivHelper,
                             private UnixSocket::ReceiveCallback,
                             private UnixSocket::SendCallback,
                             private EventBase::OnDestructionCallback {
 public:
  PrivHelperClientImpl(File&& conn, std::optional<SpawnedProcess> proc)
      : helperProc_(std::move(proc)),
        conn_(UnixSocket::makeUnique(nullptr, std::move(conn))) {}
  ~PrivHelperClientImpl() override {
    cleanup();
    DCHECK_EQ(sendPending_, 0);
  }

  void attachEventBase(EventBase* eventBase) override {
    {
      auto state = state_.wlock();
      if (state->status != Status::NOT_STARTED) {
        throw std::runtime_error(folly::to<string>(
            "PrivHelper::start() called in unexpected state ",
            static_cast<uint32_t>(state->status)));
      }
      state->eventBase = eventBase;
      state->status = Status::RUNNING;
    }
    eventBase->runOnDestruction(*this);
    conn_->attachEventBase(eventBase);
    conn_->setReceiveCallback(this);
  }

  void detachEventBase() override {
    detachWithinEventBaseDestructor();
    cancel();
  }

  Future<File> fuseMount(folly::StringPiece mountPath, bool readOnly) override;
  Future<Unit> fuseUnmount(StringPiece mountPath) override;
  Future<Unit> bindMount(StringPiece clientPath, StringPiece mountPath)
      override;
  folly::Future<folly::Unit> bindUnMount(folly::StringPiece mountPath) override;
  Future<Unit> fuseTakeoverShutdown(StringPiece mountPath) override;
  Future<Unit> fuseTakeoverStartup(
      StringPiece mountPath,
      const vector<string>& bindMounts) override;
  Future<Unit> setLogFile(folly::File logFile) override;
  Future<folly::Unit> setDaemonTimeout(
      std::chrono::nanoseconds duration) override;
  int stop() override;

 private:
  using PendingRequestMap =
      std::unordered_map<uint32_t, folly::Promise<UnixSocket::Message>>;
  enum class Status : uint32_t {
    NOT_STARTED,
    RUNNING,
    CLOSED,
    WAITED,
  };
  struct ThreadSafeData {
    Status status{Status::NOT_STARTED};
    EventBase* eventBase{nullptr};
  };

  uint32_t getNextXid() {
    return nextXid_.fetch_add(1, std::memory_order_acq_rel);
  }
  /**
   * Close the socket to the privhelper server, and wait for it to exit.
   *
   * Returns the exit status of the privhelper process, or an errno value on
   * error.
   */
  folly::Expected<ProcessStatus, int> cleanup() {
    EventBase* eventBase{nullptr};
    {
      auto state = state_.wlock();
      if (state->status == Status::WAITED) {
        // We have already waited on the privhelper process.
        return folly::makeUnexpected(ESRCH);
      }
      if (state->status == Status::RUNNING) {
        eventBase = state->eventBase;
        state->eventBase = nullptr;
      }
      state->status = Status::WAITED;
    }

    // If the state was still RUNNING detach from the EventBase.
    if (eventBase) {
      eventBase->runImmediatelyOrRunInEventBaseThreadAndWait([this] {
        conn_->clearReceiveCallback();
        conn_->detachEventBase();
        cancel();
      });
    }
    // Make sure the socket is closed, and fail any outstanding requests.
    // Closing the socket will signal the privhelper process to exit.
    closeSocket(std::runtime_error("privhelper client being destroyed"));

    // Wait until the privhelper process exits.
    if (helperProc_.has_value()) {
      return folly::makeExpected<int>(helperProc_->wait());
    } else {
      // helperProc_ can be nullopt during the unit tests, where we aren't
      // actually running the privhelper in a separate process.
      return folly::makeExpected<int>(
          ProcessStatus(ProcessStatus::State::Exited, 0));
    }
  }

  /**
   * Send a request and wait for the response.
   */
  Future<UnixSocket::Message> sendAndRecv(
      uint32_t xid,
      UnixSocket::Message&& msg) {
    EventBase* eventBase;
    {
      auto state = state_.rlock();
      if (state->status != Status::RUNNING) {
        return folly::makeFuture<UnixSocket::Message>(std::runtime_error(
            "cannot send new requests on closed privhelper connection"));
      }
      eventBase = state->eventBase;
    }

    // Note: We intentionally use EventBase::runInEventBaseThread() here rather
    // than folly::via().
    //
    // folly::via() does not do what we want, as it causes chained futures to
    // use the original executor rather than to execute inline.  In particular
    // this causes problems during destruction if the EventBase in question has
    // already been destroyed.
    folly::Promise<UnixSocket::Message> promise;
    auto future = promise.getFuture();
    eventBase->runInEventBaseThread([this,
                                     xid,
                                     msg = std::move(msg),
                                     promise = std::move(promise)]() mutable {
      // Double check that the connection is still open
      if (!conn_) {
        promise.setException(std::runtime_error(
            "cannot send new requests on closed privhelper connection"));
        return;
      }

      pendingRequests_.emplace(xid, std::move(promise));
      ++sendPending_;
      conn_->send(std::move(msg), this);
    });
    return future;
  }

  void messageReceived(UnixSocket::Message&& message) noexcept override {
    try {
      processResponse(std::move(message));
    } catch (const std::exception& ex) {
      EDEN_BUG() << "unexpected error processing privhelper response: "
                 << folly::exceptionStr(ex);
    }
  }

  void processResponse(UnixSocket::Message&& message) {
    Cursor cursor(&message.data);
    auto xid = cursor.readBE<uint32_t>();

    auto iter = pendingRequests_.find(xid);
    if (iter == pendingRequests_.end()) {
      // This normally shouldn't happen unless there is a bug.
      // We'll throw and our caller will turn this into an EDEN_BUG()
      throw std::runtime_error(folly::to<string>(
          "received unexpected response from privhelper for unknown "
          "transaction ID ",
          xid));
    }

    auto promise = std::move(iter->second);
    pendingRequests_.erase(iter);
    promise.setValue(std::move(message));
  }

  void eofReceived() noexcept override {
    handleSocketError(std::runtime_error("privhelper process exited"));
  }

  void socketClosed() noexcept override {
    handleSocketError(
        std::runtime_error("privhelper client destroyed locally"));
  }

  void receiveError(const folly::exception_wrapper& ew) noexcept override {
    // Fail all pending requests
    handleSocketError(std::runtime_error(folly::to<string>(
        "error reading from privhelper process: ", folly::exceptionStr(ew))));
  }

  void sendSuccess() noexcept override {
    --sendPending_;
  }

  void sendError(const folly::exception_wrapper& ew) noexcept override {
    // Fail all pending requests
    --sendPending_;
    handleSocketError(std::runtime_error(folly::to<string>(
        "error sending to privhelper process: ", folly::exceptionStr(ew))));
  }

  void onEventBaseDestruction() noexcept override {
    // This callback is run when the EventBase is destroyed.
    // Detach from the EventBase.  We may be restarted later if
    // attachEventBase() is called again later to attach us to a new EventBase.
    detachWithinEventBaseDestructor();
  }

  void handleSocketError(const std::exception& ex) {
    // If we are RUNNING, move to the CLOSED state and then close the socket and
    // fail all pending requests.
    //
    // If we are in any other state just return early.
    // This can occur if handleSocketError() is invoked multiple times (e.g.,
    // for a send error and a receive error).  This can happen recursively since
    // closing the socket will generally trigger any outstanding sends and
    // receives to fail.
    {
      // Exit early if the state is not RUNNING.
      // Whatever other function updated the state will have handled closing the
      // socket and failing pending requests.
      auto state = state_.wlock();
      if (state->status != Status::RUNNING) {
        return;
      }
      state->status = Status::CLOSED;
      state->eventBase = nullptr;
    }
    closeSocket(ex);
  }

  void closeSocket(const std::exception& ex) {
    PendingRequestMap pending;
    pending.swap(pendingRequests_);
    conn_.reset();
    DCHECK_EQ(sendPending_, 0);

    for (auto& entry : pending) {
      entry.second.setException(ex);
    }
  }

  // Separated out from detachEventBase() since it is not safe to cancel() an
  // EventBase::OnDestructionCallback within the callback itself.
  void detachWithinEventBaseDestructor() noexcept {
    {
      auto state = state_.wlock();
      if (state->status != Status::RUNNING) {
        return;
      }
      state->status = Status::NOT_STARTED;
      state->eventBase = nullptr;
    }
    conn_->clearReceiveCallback();
    conn_->detachEventBase();
  }

  std::optional<SpawnedProcess> helperProc_;
  std::atomic<uint32_t> nextXid_{1};
  folly::Synchronized<ThreadSafeData> state_;

  // sendPending_, pendingRequests_, and conn_ are only accessed from the
  // EventBase thread.
  size_t sendPending_{0};
  PendingRequestMap pendingRequests_;
  UnixSocket::UniquePtr conn_;
};

Future<File> PrivHelperClientImpl::fuseMount(
    StringPiece mountPath,
    bool readOnly) {
  auto xid = getNextXid();
  auto request =
      PrivHelperConn::serializeMountRequest(xid, mountPath, readOnly);
  return sendAndRecv(xid, std::move(request))
      .thenValue([](UnixSocket::Message&& response) {
        PrivHelperConn::parseEmptyResponse(
            PrivHelperConn::REQ_MOUNT_FUSE, response);
        if (response.files.size() != 1) {
          throw std::runtime_error(folly::to<string>(
              "expected privhelper FUSE response to contain a single file "
              "descriptor; got ",
              response.files.size()));
        }
        return std::move(response.files[0]);
      });
}

Future<Unit> PrivHelperClientImpl::fuseUnmount(StringPiece mountPath) {
  auto xid = getNextXid();
  auto request = PrivHelperConn::serializeUnmountRequest(xid, mountPath);
  return sendAndRecv(xid, std::move(request))
      .thenValue([](UnixSocket::Message&& response) {
        PrivHelperConn::parseEmptyResponse(
            PrivHelperConn::REQ_UNMOUNT_FUSE, response);
      });
}

Future<Unit> PrivHelperClientImpl::bindMount(
    StringPiece clientPath,
    StringPiece mountPath) {
  auto xid = getNextXid();
  auto request =
      PrivHelperConn::serializeBindMountRequest(xid, clientPath, mountPath);

  return sendAndRecv(xid, std::move(request))
      .thenValue([](UnixSocket::Message&& response) {
        PrivHelperConn::parseEmptyResponse(
            PrivHelperConn::REQ_MOUNT_BIND, response);
      });
}

folly::Future<folly::Unit> PrivHelperClientImpl::bindUnMount(
    folly::StringPiece mountPath) {
  auto xid = getNextXid();
  auto request = PrivHelperConn::serializeBindUnMountRequest(xid, mountPath);

  return sendAndRecv(xid, std::move(request))
      .thenValue([](UnixSocket::Message&& response) {
        PrivHelperConn::parseEmptyResponse(
            PrivHelperConn::REQ_UNMOUNT_BIND, response);
      });
}

Future<Unit> PrivHelperClientImpl::fuseTakeoverShutdown(StringPiece mountPath) {
  auto xid = getNextXid();
  auto request =
      PrivHelperConn::serializeTakeoverShutdownRequest(xid, mountPath);

  return sendAndRecv(xid, std::move(request))
      .thenValue([](UnixSocket::Message&& response) {
        PrivHelperConn::parseEmptyResponse(
            PrivHelperConn::REQ_TAKEOVER_SHUTDOWN, response);
      });
}

Future<Unit> PrivHelperClientImpl::fuseTakeoverStartup(
    StringPiece mountPath,
    const vector<string>& bindMounts) {
  auto xid = getNextXid();
  auto request = PrivHelperConn::serializeTakeoverStartupRequest(
      xid, mountPath, bindMounts);

  return sendAndRecv(xid, std::move(request))
      .thenValue([](UnixSocket::Message&& response) {
        PrivHelperConn::parseEmptyResponse(
            PrivHelperConn::REQ_TAKEOVER_STARTUP, response);
      });
}

Future<Unit> PrivHelperClientImpl::setLogFile(folly::File logFile) {
  auto xid = getNextXid();
  auto request =
      PrivHelperConn::serializeSetLogFileRequest(xid, std::move(logFile));

  return sendAndRecv(xid, std::move(request))
      .thenValue([](UnixSocket::Message&& response) {
        PrivHelperConn::parseEmptyResponse(
            PrivHelperConn::REQ_SET_LOG_FILE, response);
      });
}

Future<Unit> PrivHelperClientImpl::setDaemonTimeout(
    std::chrono::nanoseconds duration) {
  auto xid = getNextXid();
  auto request = PrivHelperConn::serializeSetDaemonTimeoutRequest(
      xid, std::move(duration));

  return sendAndRecv(xid, std::move(request))
      .thenValue([](UnixSocket::Message&& response) {
        PrivHelperConn::parseEmptyResponse(
            PrivHelperConn::REQ_SET_DAEMON_TIMEOUT, response);
      });
}

int PrivHelperClientImpl::stop() {
  const auto result = cleanup();
  if (result.hasError()) {
    folly::throwSystemErrorExplicit(
        result.error(), "error shutting down privhelper process");
  }
  auto status = result.value();
  if (status.killSignal() != 0) {
    return -status.killSignal();
  }
  return status.exitStatus();
}

} // unnamed namespace

unique_ptr<PrivHelper> startPrivHelper(const UserInfo& userInfo) {
  SpawnedProcess::Options opts;

  // As we are running as root, we need to be cautious about the privhelper
  // process that we are about start.
  // We require that `edenfs_privhelper` be a sibling of our executable file,
  // and that both of these paths are not symlinks, and that both are owned
  // and controlled by the same user.

  auto exePath = executablePath();
  auto canonPath = realpath(exePath.c_str());
  if (exePath != canonPath) {
    throw std::runtime_error(folly::to<std::string>(
        "Refusing to start because my exePath ",
        exePath,
        " is not the realpath to myself (which is ",
        canonPath,
        "). This is an unsafe installation and may be an indication of a "
        "symlink attack or similar attempt to escalate privileges"));
  }

  auto helperPath = exePath.dirname() + "edenfs_privhelper"_relpath;

  struct stat helperStat {};
  struct stat selfStat {};

  checkUnixError(lstat(exePath.c_str(), &selfStat), "lstat ", exePath);
  checkUnixError(lstat(helperPath.c_str(), &helperStat), "lstat ", helperPath);

  if (getuid() != geteuid()) {
    // We are a setuid binary.  Require that our executable be owned by
    // root, otherwise refuse to continue on the basis that something is
    // very fishy.
    if (selfStat.st_uid != 0) {
      throw std::runtime_error(folly::to<std::string>(
          "Refusing to start because my exePath ",
          exePath,
          "is owned by uid ",
          selfStat.st_uid,
          " rather than by root."));
    }
  }

  if (selfStat.st_uid != helperStat.st_uid ||
      selfStat.st_gid != helperStat.st_gid) {
    throw std::runtime_error(folly::to<std::string>(
        "Refusing to start because my exePath ",
        exePath,
        "is owned by uid=",
        selfStat.st_uid,
        " gid=",
        selfStat.st_gid,
        " and that doesn't match the ownership of ",
        helperPath,
        "which is owned by uid=",
        helperStat.st_uid,
        " gid=",
        helperStat.st_gid));
  }

  if (S_ISLNK(helperStat.st_mode)) {
    throw std::runtime_error(folly::to<std::string>(
        "Refusing to start because ", helperPath, " is a symlink"));
  }

  opts.executablePath(helperPath);

  File clientConn;
  File serverConn;
  PrivHelperConn::createConnPair(clientConn, serverConn);
  auto control = opts.inheritDescriptor(
      FileDescriptor(serverConn.release(), FileDescriptor::FDType::Socket));
  SpawnedProcess proc(
      {
          "edenfs_privhelper",
          // pass down identity information.
          folly::to<std::string>("--privhelper_uid=", userInfo.getUid()),
          folly::to<std::string>("--privhelper_gid=", userInfo.getGid()),
          // pass down the control pipe
          folly::to<std::string>("--privhelper_fd=", control),
      },
      std::move(opts));

  XLOG(DBG1) << "Spawned mount helper process: pid=" << proc.pid();
  return make_unique<PrivHelperClientImpl>(
      std::move(clientConn), std::move(proc));
}

unique_ptr<PrivHelper> createTestPrivHelper(File&& conn) {
  return make_unique<PrivHelperClientImpl>(std::move(conn), std::nullopt);
}

#ifdef __linux__
std::unique_ptr<PrivHelper> forkPrivHelper(
    PrivHelperServer* server,
    const UserInfo& userInfo) {
  File clientConn;
  File serverConn;
  PrivHelperConn::createConnPair(clientConn, serverConn);

  const auto pid = fork();
  checkUnixError(pid, "failed to fork mount helper");
  if (pid > 0) {
    // Parent
    serverConn.close();
    XLOG(DBG1) << "Forked mount helper process: pid=" << pid;
    return make_unique<PrivHelperClientImpl>(
        std::move(clientConn), SpawnedProcess::fromExistingProcess(pid));
  }

  // Child
  clientConn.close();
  int rc = 1;
  try {
    // Redirect stdin
    folly::File devNullIn("/dev/null", O_RDONLY);
    auto retcode = folly::dup2NoInt(devNullIn.fd(), STDIN_FILENO);
    folly::checkUnixError(retcode, "failed to redirect stdin");

    server->init(std::move(serverConn), userInfo.getUid(), userInfo.getGid());
    server->run();
    rc = 0;
  } catch (const std::exception& ex) {
    XLOG(ERR) << "error inside mount helper: " << folly::exceptionStr(ex);
  } catch (...) {
    XLOG(ERR) << "invalid type thrown inside mount helper";
  }
  _exit(rc);
}
#endif

#else // _WIN32

unique_ptr<PrivHelper> startPrivHelper(const UserInfo&) {
  return make_unique<PrivHelper>();
}

#endif // _WIN32

} // namespace eden
} // namespace facebook
