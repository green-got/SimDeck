import Network
import XCTest

final class RunnerTests: XCTestCase {
  private let queue = DispatchQueue(label: "simdeck.text-runner")
  private var listener: NWListener?
  private var lifetime: XCTestExpectation?
  private var applications: [String: XCUIApplication] = [:]

  override func setUp() {
    continueAfterFailure = true
  }

  @MainActor
  func testCommand() throws {
    let port = UInt16(ProcessInfo.processInfo.environment["SIMDECK_XCTEST_PORT"] ?? "") ?? 0
    guard port > 0, let endpointPort = NWEndpoint.Port(rawValue: port) else {
      XCTFail("SIMDECK_XCTEST_PORT is missing")
      return
    }

    lifetime = expectation(description: "SimDeck text runner lifetime")
    let parameters = NWParameters.tcp
    parameters.requiredLocalEndpoint = .hostPort(host: "127.0.0.1", port: endpointPort)
    listener = try NWListener(using: parameters)
    listener?.newConnectionHandler = { [weak self] connection in
      guard let self else { return }
      connection.start(queue: self.queue)
      self.receive(connection, data: Data())
    }
    listener?.start(queue: queue)

    guard let lifetime else { return }
    let result = XCTWaiter.wait(for: [lifetime], timeout: 24 * 60 * 60)
    if result != .completed {
      XCTFail("SimDeck text runner ended with \(result)")
    }
  }

  private func receive(_ connection: NWConnection, data: Data) {
    connection.receive(minimumIncompleteLength: 1, maximumLength: 2 * 1024 * 1024) {
      [weak self] chunk, _, _, error in
      guard let self, error == nil, let chunk else {
        connection.cancel()
        return
      }
      let combined = data + chunk
      guard let body = self.requestBody(combined) else {
        self.receive(connection, data: combined)
        return
      }
      DispatchQueue.main.async { [weak self] in
        guard let self else {
          connection.cancel()
          return
        }
        let response = self.response(for: body)
        connection.send(content: response, isComplete: true, completion: .contentProcessed { _ in
          connection.cancel()
        })
      }
    }
  }

  private func requestBody(_ data: Data) -> Data? {
    guard let boundary = data.range(of: Data("\r\n\r\n".utf8)) else { return nil }
    let header = String(decoding: data[..<boundary.lowerBound], as: UTF8.self)
    guard let contentLengthLine = header
      .components(separatedBy: "\r\n")
      .first(where: { $0.lowercased().hasPrefix("content-length:") }),
      let separator = contentLengthLine.firstIndex(of: ":"),
      let contentLength = Int(contentLengthLine[contentLengthLine.index(after: separator)...]
        .trimmingCharacters(in: .whitespacesAndNewlines)) else { return nil }
    let start = boundary.upperBound
    guard data.count >= start + contentLength else { return nil }
    return data.subdata(in: start..<(start + contentLength))
  }

  private func response(for body: Data) -> Data {
    do {
      let request = try JSONDecoder().decode(Request.self, from: body)
      let payload: [String: Any]
      switch request.command {
      case "status":
        payload = ["ok": true, "backend": "xctest"]
      case "typeText":
        guard let bundleId = request.bundleId?.trimmingCharacters(in: .whitespacesAndNewlines),
              !bundleId.isEmpty else {
          throw RunnerError.invalidBundleId
        }
        guard let text = request.text, !text.isEmpty else {
          throw RunnerError.invalidText
        }
        let app = applications[bundleId] ?? XCUIApplication(bundleIdentifier: bundleId)
        applications[bundleId] = app
        app.typeText(text)
        payload = ["ok": true]
      case "typeKey":
        guard let bundleId = request.bundleId?.trimmingCharacters(in: .whitespacesAndNewlines),
              !bundleId.isEmpty else {
          throw RunnerError.invalidBundleId
        }
        guard let key = request.key, key.count == 1 else {
          throw RunnerError.invalidKey
        }
        let modifiers = try keyModifierFlags(request.modifiers ?? 0)
        let app = applications[bundleId] ?? XCUIApplication(bundleIdentifier: bundleId)
        applications[bundleId] = app
        app.typeKey(key, modifierFlags: modifiers)
        payload = ["ok": true]
      case "shutdown":
        payload = ["ok": true]
        lifetime?.fulfill()
      default:
        throw RunnerError.invalidCommand
      }
      return http(status: 200, json: try JSONSerialization.data(withJSONObject: payload))
    } catch {
      let payload: [String: Any] = ["ok": false, "error": error.localizedDescription]
      let json = (try? JSONSerialization.data(withJSONObject: payload)) ?? Data("{\"ok\":false}".utf8)
      return http(status: 500, json: json)
    }
  }

  private func http(status: Int, json: Data) -> Data {
    var response = Data("HTTP/1.1 \(status) OK\r\nContent-Type: application/json\r\nContent-Length: \(json.count)\r\nConnection: close\r\n\r\n".utf8)
    response.append(json)
    return response
  }
}

private struct Request: Decodable {
  let command: String
  let bundleId: String?
  let key: String?
  let modifiers: UInt?
  let text: String?
}

private func keyModifierFlags(_ rawValue: UInt) throws -> XCUIElement.KeyModifierFlags {
  guard rawValue & ~0b10_1111 == 0 else {
    throw RunnerError.invalidModifiers
  }
  var modifiers: XCUIElement.KeyModifierFlags = []
  if rawValue & 0b000001 != 0 { modifiers.insert(.shift) }
  if rawValue & 0b000010 != 0 { modifiers.insert(.control) }
  if rawValue & 0b000100 != 0 { modifiers.insert(.option) }
  if rawValue & 0b001000 != 0 { modifiers.insert(.command) }
  if rawValue & 0b100000 != 0 { modifiers.insert(.function) }
  return modifiers
}

private enum RunnerError: LocalizedError {
  case invalidBundleId
  case invalidText
  case invalidKey
  case invalidModifiers
  case invalidCommand

  var errorDescription: String? {
    switch self {
    case .invalidBundleId: return "command requires bundleId"
    case .invalidText: return "typeText requires non-empty text"
    case .invalidKey: return "typeKey requires exactly one character"
    case .invalidModifiers: return "typeKey contains unsupported modifiers"
    case .invalidCommand: return "unsupported runner command"
    }
  }
}
