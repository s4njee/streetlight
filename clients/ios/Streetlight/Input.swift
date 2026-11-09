import GameController
import UIKit
import StreetlightCore

/// Touch surface over the video. Default "pointer" mode:
///   1 finger: move the cursor to the finger and hold left button (tap = click)
///   2-finger tap: right click · 2-finger drag: scroll · 3-finger tap: back to the menu
/// "Touchscreen" mode passes raw multitouch to the host instead.
final class TouchInputView: UIView {
    weak var client: CoreClient?
    var streamSize = CGSize(width: 1920, height: 1080)
    var touchscreenMode = false
    var onMenu: (() -> Void)?

    private var ids: [ObjectIdentifier: UInt8] = [:]
    private var twoFingerStart: (time: TimeInterval, moved: Bool, lastY: CGFloat, lastX: CGFloat)?
    private var leftDown = false

    override init(frame: CGRect) {
        super.init(frame: frame)
        isMultipleTouchEnabled = true
        backgroundColor = .clear
    }

    required init?(coder: NSCoder) { fatalError() }

    /// View point → 0..65535 over the aspect-fit video rect.
    private func norm(_ p: CGPoint) -> (UInt16, UInt16) {
        let scale = min(bounds.width / streamSize.width, bounds.height / streamSize.height)
        let w = streamSize.width * scale, h = streamSize.height * scale
        let ox = (bounds.width - w) / 2, oy = (bounds.height - h) / 2
        let nx = min(max((p.x - ox) / w, 0), 1), ny = min(max((p.y - oy) / h, 0), 1)
        return (UInt16(nx * 65535), UInt16(ny * 65535))
    }

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        guard let c = client?.ptr else { return }
        let all = event?.allTouches?.filter { $0.phase != .ended && $0.phase != .cancelled } ?? touches
        if touchscreenMode {
            for t in touches {
                let id = UInt8(ids.count % 10)
                ids[ObjectIdentifier(t)] = id
                let (x, y) = norm(t.location(in: self))
                sl_input_touch(c, id, 0, x, y)
            }
            return
        }
        if all.count == 1, let t = touches.first, t.type != .indirectPointer {
            let (x, y) = norm(t.location(in: self))
            sl_input_mouse_abs(c, x, y)
            sl_input_mouse_button(c, 0, true)
            leftDown = true
        } else if all.count == 2 {
            if leftDown { sl_input_mouse_button(c, 0, false); leftDown = false }
            let p = centroid(all)
            twoFingerStart = (event?.timestamp ?? 0, false, p.y, p.x)
        } else if all.count == 3 {
            onMenu?()
        }
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        guard let c = client?.ptr else { return }
        if touchscreenMode {
            for t in touches {
                guard let id = ids[ObjectIdentifier(t)] else { continue }
                let (x, y) = norm(t.location(in: self))
                sl_input_touch(c, id, 1, x, y)
            }
            return
        }
        let all = event?.allTouches?.filter { $0.phase != .ended && $0.phase != .cancelled } ?? touches
        if all.count == 1, leftDown, let t = touches.first {
            let (x, y) = norm(t.location(in: self))
            sl_input_mouse_abs(c, x, y)
        } else if all.count == 2, var s = twoFingerStart {
            let p = centroid(all)
            let dy = p.y - s.lastY, dx = p.x - s.lastX
            if abs(dy) + abs(dx) > 1 {
                s.moved = true
                sl_input_scroll(c, Int32(-dx * 6), Int32(dy * 6))
                s.lastY = p.y
                s.lastX = p.x
                twoFingerStart = s
            }
        }
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        guard let c = client?.ptr else { return }
        if touchscreenMode {
            for t in touches {
                guard let id = ids.removeValue(forKey: ObjectIdentifier(t)) else { continue }
                let (x, y) = norm(t.location(in: self))
                sl_input_touch(c, id, 2, x, y)
            }
            return
        }
        if leftDown { sl_input_mouse_button(c, 0, false); leftDown = false }
        if let s = twoFingerStart, (event?.timestamp ?? 0) - s.time < 0.3, !s.moved {
            sl_input_mouse_button(c, 1, true)
            sl_input_mouse_button(c, 1, false)
        }
        let remaining = event?.allTouches?.filter { $0.phase != .ended && $0.phase != .cancelled }.count ?? 0
        if remaining == 0 { twoFingerStart = nil }
    }

    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        touchesEnded(touches, with: event)
    }

    private func centroid(_ t: Set<UITouch>) -> CGPoint {
        let pts = t.map { $0.location(in: self) }
        return CGPoint(x: pts.map(\.x).reduce(0, +) / CGFloat(pts.count), y: pts.map(\.y).reduce(0, +) / CGFloat(pts.count))
    }
}

/// HID keyboard usage (UIKeyboardHIDUsage) → Linux evdev KEY_* (from Linux usbkbd.c).
let hidToEvdev: [UInt16] = [
    0, 0, 0, 0, 30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38,
    50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44, 2, 3,
    4, 5, 6, 7, 8, 9, 10, 11, 28, 1, 14, 15, 57, 12, 13, 26,
    27, 43, 43, 39, 40, 41, 51, 52, 53, 58, 59, 60, 61, 62, 63, 64,
    65, 66, 67, 68, 87, 88, 99, 70, 119, 110, 102, 104, 111, 107, 109, 106,
    105, 108, 103, 69, 98, 55, 74, 78, 96, 79, 80, 81, 75, 76, 77, 71,
    72, 73, 82, 83, 86, 127, 116, 117, 183, 184, 185, 186, 187, 188, 189, 190,
    191, 192, 193, 194, 134, 138, 130, 132, 128, 129, 131, 137, 133, 135, 136, 113,
    115, 114, 0, 0, 0, 121, 0, 89, 93, 124, 92, 94, 95, 0, 0, 0,
    122, 123, 90, 91, 85, 0, 0, 0, 0, 0, 0, 0, 111, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    29, 42, 56, 125, 97, 54, 100, 126,
]

func evdevCode(for key: UIKey) -> UInt16? {
    let usage = Int(key.keyCode.rawValue)
    guard usage < hidToEvdev.count, hidToEvdev[usage] != 0 else { return nil }
    return hidToEvdev[usage]
}

/// Mice (relative motion while the pointer is locked) and game controllers.
final class PeripheralInput {
    weak var client: CoreClient?
    private var pads: [ObjectIdentifier: UInt8] = [:]
    private var observers: [NSObjectProtocol] = []

    init() {
        let nc = NotificationCenter.default
        observers.append(nc.addObserver(forName: .GCMouseDidConnect, object: nil, queue: .main) { [weak self] n in
            if let m = n.object as? GCMouse { self?.attach(mouse: m) }
        })
        observers.append(nc.addObserver(forName: .GCControllerDidConnect, object: nil, queue: .main) { [weak self] n in
            if let c = n.object as? GCController { self?.attach(pad: c) }
        })
        observers.append(nc.addObserver(forName: .GCControllerDidDisconnect, object: nil, queue: .main) { [weak self] n in
            guard let self, let c = n.object as? GCController, let id = self.pads.removeValue(forKey: ObjectIdentifier(c)),
                  let p = self.client?.ptr else { return }
            sl_input_gamepad_disconnected(p, id)
        })
        GCMouse.mice().forEach(attach(mouse:))
        GCController.controllers().forEach(attach(pad:))
    }

    deinit { observers.forEach(NotificationCenter.default.removeObserver) }

    private func attach(mouse: GCMouse) {
        guard let input = mouse.mouseInput else { return }
        input.mouseMovedHandler = { [weak self] _, dx, dy in
            guard let p = self?.client?.ptr else { return }
            sl_input_mouse_move(p, Int32(dx.rounded()), Int32((-dy).rounded()))
        }
        let button = { [weak self] (b: UInt8) -> GCControllerButtonValueChangedHandler in
            { _, _, pressed in if let p = self?.client?.ptr { sl_input_mouse_button(p, b, pressed) } }
        }
        input.leftButton.pressedChangedHandler = button(0)
        input.rightButton?.pressedChangedHandler = button(1)
        input.middleButton?.pressedChangedHandler = button(2)
        if let aux = input.auxiliaryButtons {
            for (i, b) in aux.prefix(2).enumerated() { b.pressedChangedHandler = button(UInt8(3 + i)) }
        }
        input.scroll.valueChangedHandler = { [weak self] _, x, y in
            guard let p = self?.client?.ptr else { return }
            sl_input_scroll(p, Int32(x * 120), Int32(y * 120))
        }
    }

    private func attach(pad c: GCController) {
        guard let g = c.extendedGamepad else { return }
        let id = UInt8(pads.count)
        pads[ObjectIdentifier(c)] = id
        g.valueChangedHandler = { [weak self] gp, _ in
            guard let p = self?.client?.ptr else { return }
            var b: UInt32 = 0
            func set(_ on: Bool, _ bit: UInt32) { if on { b |= bit } }
            set(gp.buttonA.isPressed, 1 << 0); set(gp.buttonB.isPressed, 1 << 1)
            set(gp.buttonX.isPressed, 1 << 2); set(gp.buttonY.isPressed, 1 << 3)
            set(gp.leftShoulder.isPressed, 1 << 4); set(gp.rightShoulder.isPressed, 1 << 5)
            set(gp.buttonOptions?.isPressed ?? false, 1 << 6); set(gp.buttonMenu.isPressed, 1 << 7)
            set(gp.buttonHome?.isPressed ?? false, 1 << 8)
            set(gp.leftThumbstickButton?.isPressed ?? false, 1 << 9); set(gp.rightThumbstickButton?.isPressed ?? false, 1 << 10)
            set(gp.dpad.up.isPressed, 1 << 11); set(gp.dpad.down.isPressed, 1 << 12)
            set(gp.dpad.left.isPressed, 1 << 13); set(gp.dpad.right.isPressed, 1 << 14)
            let ax = { (v: Float) in Int16(max(-1, min(1, v)) * 32767) }
            sl_input_gamepad(p, id, b, ax(gp.leftThumbstick.xAxis.value), ax(gp.leftThumbstick.yAxis.value),
                             ax(gp.rightThumbstick.xAxis.value), ax(gp.rightThumbstick.yAxis.value),
                             UInt8(gp.leftTrigger.value * 255), UInt8(gp.rightTrigger.value * 255))
        }
    }
}
