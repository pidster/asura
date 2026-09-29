import Darwin
import Dispatch
import Foundation

/// State is confined to `queue`; only immutable stream handles cross that queue.
public final class Transport: @unchecked Sendable {
    public let frames: AsyncThrowingStream<Data, any Error>
    private let continuation: AsyncThrowingStream<Data, any Error>.Continuation
    private let queue = DispatchQueue(label: "asura.model.io")
    private let fd: Int32
    private var reader: DispatchSourceRead?
    private var writer: DispatchSourceWrite?
    private var writerSuspended = true
    private var closed = false
    private var finishing = false
    private var decoder = FrameDecoder()
    private var controls: [Data] = []
    private var dataFrames: [Data] = []
    private var current = Data()
    private var offset = 0

    public init(fd: Int32) throws {
        var kind: Int32 = 0
        var size = socklen_t(MemoryLayout<Int32>.size)
        guard getsockopt(fd, SOL_SOCKET, SO_TYPE, &kind, &size) == 0, kind == SOCK_STREAM else {
            throw HelperError.protocolFault
        }
        let flags = fcntl(fd, F_GETFL)
        guard flags >= 0, fcntl(fd, F_SETFL, flags | O_NONBLOCK) == 0,
            fcntl(fd, F_SETFD, FD_CLOEXEC) == 0 else { throw HelperError.unavailable }
        var yes: Int32 = 1
        guard setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &yes, socklen_t(MemoryLayout<Int32>.size)) == 0 else {
            throw HelperError.unavailable
        }
        self.fd = fd
        let pair = AsyncThrowingStream<Data, any Error>.makeStream(bufferingPolicy: .bufferingOldest(8))
        frames = pair.stream
        continuation = pair.continuation
        queue.async { [self] in
            let read = DispatchSource.makeReadSource(fileDescriptor: fd, queue: queue)
            read.setEventHandler { [self] in readAvailable() }
            reader = read
            let write = DispatchSource.makeWriteSource(fileDescriptor: fd, queue: queue)
            write.setEventHandler { [self] in writeAvailable() }
            writer = write
            read.resume()
        }
    }

    /// Enqueues bounded output without waiting on a slow peer.
    public func send(_ message: Envelope, control: Bool = true) async throws {
        let frame = try Wire.frame(message)
        try await withCheckedThrowingContinuation { (done: CheckedContinuation<Void, any Error>) in
            queue.async { [self] in
                guard !closed, !finishing else { done.resume(throwing: HelperError.closed); return }
                if control {
                    guard frame.count <= 4096, controls.count < 8 else {
                        done.resume(throwing: HelperError.limit); fail(HelperError.limit); return
                    }
                    controls.append(frame)
                } else {
                    guard dataFrames.count < 8,
                        dataFrames.reduce(0, { $0 + $1.count }) + frame.count <= Limits.frame + 1024 else {
                        done.resume(throwing: HelperError.limit); fail(HelperError.limit); return
                    }
                    dataFrames.append(frame)
                }
                if writerSuspended { writerSuspended = false; writer?.resume() }
                done.resume()
            }
        }
    }

    public func close() { queue.async { [self] in fail(nil) } }
    public func finish() {
        queue.async { [self] in
            guard !closed else { return }
            finishing = true
            reader?.setEventHandler(handler: nil)
            reader?.cancel()
            reader = nil
            writeAvailable()
        }
    }

    private func readAvailable() {
        guard !closed else { return }
        var bytes = [UInt8](repeating: 0, count: Limits.chunk)
        for _ in 0..<16 {
            let count = Darwin.read(fd, &bytes, bytes.count)
            if count == 0 { fail(decoder.isEmpty ? nil : HelperError.protocolFault); return }
            if count < 0 {
                if errno == EAGAIN || errno == EWOULDBLOCK { return }
                if errno == EINTR { continue }
                fail(HelperError.closed); return
            }
            do {
                for frame in try decoder.feed(Data(bytes.prefix(count))) {
                    switch continuation.yield(frame) {
                    case .enqueued: break
                    case .dropped, .terminated: throw HelperError.limit
                    @unknown default: throw HelperError.protocolFault
                    }
                }
            } catch { fail(error); return }
        }
    }

    private func writeAvailable() {
        guard !closed else { return }
        var budget = 65_536
        for _ in 0..<16 {
            if current.isEmpty {
                if !controls.isEmpty { current = controls.removeFirst() }
                else if !dataFrames.isEmpty { current = dataFrames.removeFirst() }
                else {
                    if finishing { fail(nil) }
                    else if !writerSuspended { writerSuspended = true; writer?.suspend() }
                    return
                }
                offset = 0
            }
            let count = current.withUnsafeBytes { raw in
                Darwin.write(fd, raw.baseAddress!.advanced(by: offset), min(current.count - offset, budget))
            }
            if count < 0 {
                if errno == EAGAIN || errno == EWOULDBLOCK { return }
                if errno == EINTR { continue }
                fail(HelperError.closed); return
            }
            guard count > 0 else { fail(HelperError.closed); return }
            offset += count
            budget -= count
            if offset == current.count { current.removeAll(keepingCapacity: false); offset = 0 }
            if budget == 0 { return }
        }
    }

    private func fail(_ error: (any Error)?) {
        guard !closed else { return }
        closed = true
        reader?.setEventHandler(handler: nil)
        reader?.cancel()
        writer?.setEventHandler(handler: nil)
        if writerSuspended { writerSuspended = false; writer?.resume() }
        writer?.cancel()
        reader = nil
        writer = nil
        controls.removeAll(); dataFrames.removeAll(); current.removeAll()
        continuation.finish(throwing: error)
        // All IO callbacks run on this queue and observe `closed` before use.
        Darwin.close(fd)
    }
}
