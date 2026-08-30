import AppKit
import XCTest

@testable import TcrBarCore

/// Records every begin/end the controller performs, so the *pairing* can be
/// asserted.
///
/// The pairing is the whole safety property. A leaked token cannot be released
/// without quitting the app, and the panel would then read OFF while the Mac
/// still refused to sleep — a control lying about the state of the machine.
/// Nothing in-process can observe a real power assertion, so this fake is the
/// only place that property is checkable at all.
@MainActor
final class RecordingActivity {
    private(set) var begun: [NSObjectProtocol] = []
    private(set) var ended: [NSObjectProtocol] = []
    private(set) var reasons: [String] = []

    /// Tokens still outstanding — begun and not ended. The number that must
    /// never exceed one.
    var live: [NSObjectProtocol] {
        begun.filter { token in !ended.contains { $0 === token } }
    }

    var activity: AwakeController.Activity {
        AwakeController.Activity(
            begin: { [self] reason in
                reasons.append(reason)
                // A fresh object per call, so "ended the token it began" is a
                // statement about identity and not about equality.
                let token = NSObject()
                begun.append(token)
                return token
            },
            end: { [self] token in ended.append(token) }
        )
    }
}

@MainActor
final class AwakeControllerTests: XCTestCase {

    func testOffToOnBeginsExactlyOneActivity() {
        let fake = RecordingActivity()
        let controller = AwakeController(activity: fake.activity, defaults: nil)

        controller.setOn(true)

        XCTAssertEqual(fake.begun.count, 1)
        XCTAssertEqual(fake.ended.count, 0)
        XCTAssertTrue(controller.isOn)
    }

    /// The leak case, and the reason ``AwakeController/begin()`` guards.
    ///
    /// A second `beginActivity` returns a second token that this class has
    /// nowhere to put, so the first becomes unreleasable for the lifetime of the
    /// process.
    func testOnToOnDoesNotBeginASecondActivity() {
        let fake = RecordingActivity()
        let controller = AwakeController(activity: fake.activity, defaults: nil)

        controller.setOn(true)
        controller.setOn(true)
        controller.setOn(true)

        XCTAssertEqual(fake.begun.count, 1, "a second token would be unreleasable")
        XCTAssertEqual(fake.live.count, 1)
        XCTAssertTrue(controller.isOn)
    }

    func testOnToOffEndsExactlyTheTokenThatWasBegun() {
        let fake = RecordingActivity()
        let controller = AwakeController(activity: fake.activity, defaults: nil)

        controller.setOn(true)
        controller.setOn(false)

        XCTAssertEqual(fake.ended.count, 1)
        XCTAssertTrue(
            fake.ended.first === fake.begun.first,
            "must end the identical token, not merely an equal one")
        XCTAssertTrue(fake.live.isEmpty)
        XCTAssertFalse(controller.isOn)
    }

    /// Ending nothing is not the same as ending something. `endActivity` on a
    /// token this app did not begin is undefined behaviour, and the simplest way
    /// to reach it is a spurious off.
    func testOffToOffEndsNothing() {
        let fake = RecordingActivity()
        let controller = AwakeController(activity: fake.activity, defaults: nil)

        controller.setOn(false)
        controller.setOn(false)

        XCTAssertEqual(fake.begun.count, 0)
        XCTAssertEqual(fake.ended.count, 0)
        XCTAssertFalse(controller.isOn)
    }

    /// `isOn` is a mirror of the live token, never an independent flag. Walked
    /// across every transition, including the two no-ops, because a divergence
    /// on any single one of them is a panel that reports a state of the machine
    /// that is not true.
    func testIsOnTracksTheLiveTokenAcrossEveryTransition() {
        let fake = RecordingActivity()
        let controller = AwakeController(activity: fake.activity, defaults: nil)

        for on in [false, true, true, false, false, true, false] {
            controller.setOn(on)
            XCTAssertEqual(
                controller.isOn, !fake.live.isEmpty,
                "isOn=\(controller.isOn) but \(fake.live.count) token(s) held")
        }
    }

    func testToggleFlipsAndReleases() {
        let fake = RecordingActivity()
        let controller = AwakeController(activity: fake.activity, defaults: nil)

        controller.toggle()
        XCTAssertTrue(controller.isOn)
        controller.toggle()
        XCTAssertFalse(controller.isOn)
        XCTAssertTrue(fake.live.isEmpty)
    }

    func testReleaseOnQuitEndsAHeldActivityAndIsSafeWhenNoneIsHeld() {
        let fake = RecordingActivity()
        let controller = AwakeController(activity: fake.activity, defaults: nil)

        controller.releaseOnQuit()
        XCTAssertEqual(fake.ended.count, 0, "nothing was held, so nothing may be ended")

        controller.setOn(true)
        controller.releaseOnQuit()
        XCTAssertEqual(fake.ended.count, 1)
        XCTAssertFalse(controller.isOn)

        controller.releaseOnQuit()
        XCTAssertEqual(fake.ended.count, 1, "a second quit must not double-end")
    }

    /// The reason string is what `pmset -g assertions` shows and what a human
    /// greps for when their Mac will not sleep. Rewording it silently breaks
    /// both that search and the README's gate.
    func testReasonNamesTheAppSoItIsGreppable() {
        let fake = RecordingActivity()
        let controller = AwakeController(activity: fake.activity, defaults: nil)

        controller.setOn(true)

        XCTAssertEqual(fake.reasons, [AwakeController.reason])
        XCTAssertTrue(AwakeController.reason.contains("TcrBar"), AwakeController.reason)
    }

    /// The set is the point of the class: `caffeinate -i -m -s`, not `-i` alone.
    ///
    /// Spelled as literals rather than re-derived from the same constants the
    /// production code uses, because a test that reads its expectation out of
    /// the thing under test agrees with any change to it. These three strings
    /// are what `pmset -g assertions` prints and what the README's gate counts.
    func testTheThreeAssertionTypesAreTheOnesCaffeinateHolds() {
        XCTAssertEqual(
            AwakeController.assertionTypes,
            ["PreventUserIdleSystemSleep", "PreventSystemSleep", "PreventDiskIdle"])
    }

    /// A take that fails reports OFF, not partially ON.
    ///
    /// `IOPMAssertionCreateWithName` returns an `IOReturn` and can fail, which
    /// `beginActivity` could not. A controller that showed ON having taken some
    /// smaller set than it promised would be the panel lying about the state of
    /// the machine — the defect the single-token shape exists to prevent.
    func testAFailedTakeLeavesTheControlOffAndRetryable() {
        var attempts = 0
        var ended = 0
        let activity = AwakeController.Activity(
            begin: { _ in
                attempts += 1
                return nil
            },
            end: { _ in ended += 1 })
        let controller = AwakeController(activity: activity, defaults: nil)

        controller.setOn(true)
        XCTAssertFalse(controller.isOn, "nothing was taken, so the control must read OFF")
        XCTAssertEqual(ended, 0, "there is nothing to end")

        // Not latched into a broken state: the next tick tries again, and the
        // idempotence guard does not mistake a failed take for a live one.
        controller.setOn(true)
        XCTAssertEqual(attempts, 2)
        XCTAssertFalse(controller.isOn)
    }

    /// The harness pair must hold nothing. If `.inert` ever became the real one,
    /// rendering PNGs would stop the machine sleeping.
    func testInertActivityIsSafeForTheRenderHarness() {
        let controller = AwakeController(activity: .inert, defaults: nil)
        controller.setOn(true)
        XCTAssertTrue(controller.isOn, "the harness still needs the ON appearance")
        controller.setOn(false)
        XCTAssertFalse(controller.isOn)
    }
}

// MARK: - Probe argument parsing

@MainActor
final class KeepAwakeProbeTests: XCTestCase {

    func testAbsentFlagLeavesTheAppToStartNormally() {
        XCTAssertNil(KeepAwakeProbe.request(["TcrBar"]))
        XCTAssertNil(KeepAwakeProbe.request(["TcrBar", "--render-states", "/tmp/x"]))
    }

    func testDurationIsParsed() {
        XCTAssertEqual(
            KeepAwakeProbe.request(["TcrBar", KeepAwakeProbe.flag, "10"]),
            .hold(seconds: 10))
        XCTAssertEqual(
            KeepAwakeProbe.request(["TcrBar", KeepAwakeProbe.flag, "0.5"]),
            .hold(seconds: 0.5))
    }

    /// A broken argument is NOT the same as an absent one. Falling back to `nil`
    /// would launch a menu-bar app in answer to a typo, hiding the mistake
    /// behind an icon.
    func testBadDurationsAreUsageErrorsRatherThanASilentLaunch() {
        for bad in ["", "0", "-5", "ten", "10s"] {
            guard case .usage = KeepAwakeProbe.request(["TcrBar", KeepAwakeProbe.flag, bad])
            else { return XCTFail("'\(bad)' should be a usage error, not a duration") }
        }
        guard case .usage = KeepAwakeProbe.request(["TcrBar", KeepAwakeProbe.flag])
        else { return XCTFail("a missing duration should be a usage error") }
    }

    /// `Double("inf")` parses. A probe asked to hold for infinity would never
    /// release — the exact failure the probe exists to detect, committed by the
    /// probe itself.
    func testInfinityIsRejected() {
        for bad in ["inf", "-inf", "nan", "1e400"] {
            guard case .usage = KeepAwakeProbe.request(["TcrBar", KeepAwakeProbe.flag, bad])
            else { return XCTFail("'\(bad)' parses as a Double and must still be refused") }
        }
    }
}

// MARK: - The menu-bar mark

@MainActor
final class KeepAwakeGlyphTests: XCTestCase {

    /// A status item renders a template image in the menu bar's own colour,
    /// which strips the tint. `isTemplate = false` is the opt-out, and it is the
    /// single line that makes the colour channel possible at all.
    func testTheMarkIsNotATemplate() throws {
        let image = try XCTUnwrap(KeepAwakeGlyph.image(tint: .systemRed))
        XCTAssertFalse(image.isTemplate)
    }

    /// Proves the image THIS APP HANDS OVER carries colour: rasterise it and
    /// find a pixel that is actually the tint.
    ///
    /// It does not prove the status item honours it. Nothing here can — reading
    /// back the real menu bar needs Screen Recording, which is not granted on
    /// the machine this was written on. That step is a human's eyes.
    ///
    /// The tint is pure saturated red rather than `Tok.awake` on purpose: the
    /// assertion is about colour surviving, so it must not be able to pass on a
    /// near-grey, and a token is free to change.
    func testAPixelInsideTheMarkCarriesTheTint() throws {
        let tint = NSColor(srgbRed: 1, green: 0, blue: 0, alpha: 1)
        let image = try XCTUnwrap(KeepAwakeGlyph.image(tint: tint))

        let side = 64
        let rep = try XCTUnwrap(
            NSBitmapImageRep(
                bitmapDataPlanes: nil, pixelsWide: side, pixelsHigh: side,
                bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0))
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
        image.draw(
            in: NSRect(x: 0, y: 0, width: side, height: side),
            from: .zero, operation: .sourceOver, fraction: 1)
        NSGraphicsContext.restoreGraphicsState()

        var reddest: (r: CGFloat, g: CGFloat, b: CGFloat, a: CGFloat)?
        var opaquePixels = 0
        for x in 0..<side {
            for y in 0..<side {
                guard let px = rep.colorAt(x: x, y: y),
                    let srgb = px.usingColorSpace(.sRGB), srgb.alphaComponent > 0.5
                else { continue }
                opaquePixels += 1
                let candidate = (
                    srgb.redComponent, srgb.greenComponent, srgb.blueComponent,
                    srgb.alphaComponent
                )
                if reddest == nil || candidate.0 > reddest!.r { reddest = candidate }
            }
        }

        // A positive control on the probe itself: if nothing was drawn, the
        // colour assertion below would be vacuous rather than failing.
        XCTAssertGreaterThan(opaquePixels, 0, "nothing was rasterised — the scan proves nothing")

        let hit = try XCTUnwrap(reddest, "no opaque pixel found")
        XCTAssertGreaterThan(hit.r, 0.5, "no red in the mark — the tint was dropped")
        XCTAssertLessThan(hit.g, 0.4, "the mark is not the tint it was given")
        XCTAssertLessThan(hit.b, 0.4, "the mark is not the tint it was given")
    }

    /// The shape channel: the mark must not be the capacity gauge. They are told
    /// apart by silhouette before colour is involved.
    func testTheMarkIsNotAGauge() {
        XCTAssertFalse(KeepAwakeGlyph.symbolName.contains("gauge"))
        XCTAssertFalse(KeepAwakeGlyph.accessibilityDescription.isEmpty)
    }
}
