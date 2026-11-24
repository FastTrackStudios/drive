// The agent, as Swift sees it.
//
// Every question the extension has goes to the running agent through
// `drive-fileprovider` (a Rust static library with a C ABI): a request as
// JSON in, its answer as JSON out (docs/fileprovider.md). Nothing here
// decides anything; it is a typed wrapper over that one call.

import FileProvider
import Foundation

/// The app group the extension and the agent share: the agent serves its
/// app API on a socket in this group's container.
let appGroup = "28C2G63DA7.app.fasttrackstudio.drive"

/// A call the agent refused or could not be asked, with its reason.
struct AgentError: LocalizedError {
    let reason: String
    var errorDescription: String? { reason }

    /// The reason the last call on this thread left, or a stand-in: a
    /// failure without one is still a failure (swallowing it would leave
    /// Finder showing a spinner).
    static func last(_ fallback: String) -> AgentError {
        guard let raw = dfp_last_error() else { return AgentError(reason: fallback) }
        defer { dfp_free(raw) }
        return AgentError(reason: String(cString: raw))
    }

    /// The agent's "exists: <name>": a name taken in that folder.
    var isCollision: Bool { reason.hasPrefix("exists") }
    /// The agent's "no item …".
    var isMissing: Bool { reason.hasPrefix("no item") }
}

/// Any error, as the system accepts it: only NSFileProviderErrorDomain and
/// NSCocoaErrorDomain. Anything else it refuses ("unsupported error
/// domain") and the call fails with no reason anyone can see.
func providerError(_ error: Error) -> Error {
    let ns = error as NSError
    if ns.domain == NSFileProviderErrorDomain || ns.domain == NSCocoaErrorDomain {
        return error
    }
    if let e = error as? AgentError {
        if e.isMissing { return NSFileProviderError(.noSuchItem) }
        if e.isCollision { return NSFileProviderError(.filenameCollision) }
        if e.reason.hasPrefix("the Drive agent") { return NSFileProviderError(.serverUnreachable) }
        return NSError(domain: NSCocoaErrorDomain, code: NSFileReadUnknownError,
                       userInfo: [NSLocalizedDescriptionKey: e.reason])
    }
    return NSError(domain: NSCocoaErrorDomain, code: NSFileReadUnknownError,
                   userInfo: [NSLocalizedDescriptionKey: ns.localizedDescription])
}

/// One item of the Drive tree (`drive_local::FpItem`).
struct DriveItem: Decodable {
    let id: String
    let parent: String
    let name: String
    let dir: Bool
    let size: UInt64
    let mtime: Int64
    let content: String
    let meta: String
    let here: Bool
}

struct DrivePage: Decodable {
    let items: [DriveItem]
    let next: String?
}

struct DriveChanges: Decodable {
    let updated: [DriveItem]
    let deleted: [String]
    let anchor: String
    let expired: Bool?
}

struct DriveFetched: Decodable {
    let path: String
    let item: DriveItem
}

private struct Anchor: Decodable { let anchor: String }

enum Agent {
    /// The group container: where the socket is, and where fetched bytes
    /// and outgoing edits are staged (the agent may not read the
    /// extension's own container, and the extension may not read the
    /// agent's cache).
    static var container: URL? {
        FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: appGroup)
    }

    static var staging: URL? {
        container?.appendingPathComponent("staging", isDirectory: true)
    }

    private static let connected: Bool = {
        guard let sock = container?.appendingPathComponent("app.sock").path else { return false }
        return dfp_connect(sock) == 0
    }()

    /// One request; its answer as the agent's JSON text.
    static func raw(_ request: [String: Any]) throws -> String {
        if !connected {
            // Connecting again each call: the agent may have started since.
            if let sock = container?.appendingPathComponent("app.sock").path, dfp_connect(sock) != 0 {
                throw AgentError.last("the Drive agent is not running")
            }
        }
        let body = try JSONSerialization.data(withJSONObject: request)
        guard let text = String(data: body, encoding: .utf8) else {
            throw AgentError(reason: "a request that is not text")
        }
        guard let answer = dfp_call(text) else {
            throw AgentError.last("the Drive agent did not answer")
        }
        defer { dfp_free(answer) }
        return String(cString: answer)
    }

    /// One request; its answer decoded.
    static func call<T: Decodable>(_ request: [String: Any], as _: T.Type) throws -> T {
        try JSONDecoder().decode(T.self, from: Data(try raw(request).utf8))
    }

    /// One request whose answer is nothing.
    static func send(_ request: [String: Any]) throws {
        _ = try raw(request)
    }

    static func item(_ id: String) throws -> DriveItem {
        try call(["op": "fp_item", "item": id], as: DriveItem.self)
    }

    static func list(_ parent: String, after: String?) throws -> DrivePage {
        var req: [String: Any] = ["op": "fp_list", "parent": parent]
        if let after { req["after"] = after }
        return try call(req, as: DrivePage.self)
    }

    static func anchor() throws -> String {
        try call(["op": "fp_anchor"], as: Anchor.self).anchor
    }

    static func changes(since anchor: String) throws -> DriveChanges {
        try call(["op": "fp_changes", "anchor": anchor], as: DriveChanges.self)
    }

    static func fetch(_ id: String) throws -> DriveFetched {
        guard let into = staging?.path else { throw AgentError(reason: "no app group container") }
        return try call(["op": "fp_fetch", "item": id, "into": into], as: DriveFetched.self)
    }

    static func create(parent: String, name: String, dir: Bool, from: String?) throws -> DriveItem {
        var req: [String: Any] = ["op": "fp_create", "parent": parent, "name": name, "dir": dir]
        if let from { req["from"] = from }
        return try call(req, as: DriveItem.self)
    }

    static func modify(_ id: String, name: String?, parent: String?, from: String?) throws -> DriveItem {
        var req: [String: Any] = ["op": "fp_modify", "item": id]
        if let name { req["name"] = name }
        if let parent { req["parent"] = parent }
        if let from { req["from"] = from }
        return try call(req, as: DriveItem.self)
    }

    static func delete(_ id: String) throws {
        try send(["op": "fp_delete", "item": id])
    }

    /// Copy what the system hands over into the group container, where the
    /// agent can read it. The caller removes it once the agent has.
    static func stage(_ url: URL) throws -> URL {
        guard let dir = staging else { throw AgentError(reason: "no app group container") }
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let copy = dir.appendingPathComponent("up-\(UUID().uuidString)")
        try FileManager.default.copyItem(at: url, to: copy)
        return copy
    }
}
