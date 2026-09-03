import { afterEach, describe, expect, it, vi } from "vitest";

import { bootSimulator, startSimulatorScreenRecording } from "./controls";

describe("controls", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("posts Android emulator startup args when booting", async () => {
    const fetchMock = vi.fn(async () => {
      return new Response(JSON.stringify({ ok: true, simulator: null }), {
        headers: { "content-type": "application/json" },
        status: 200,
      });
    });
    vi.stubGlobal("fetch", fetchMock);

    await bootSimulator("android:Pixel_8_API_36", {
      androidEmulatorArgs: ["-no-snapshot"],
      androidDisableAudio: false,
    });

    expect(fetchMock).toHaveBeenCalledWith(
      "/api/simulators/android:Pixel_8_API_36/boot",
      expect.objectContaining({
        body: JSON.stringify({
          androidEmulatorArgs: ["-no-snapshot"],
          androidDisableAudio: false,
        }),
        method: "POST",
      }),
    );
  });

  it("starts screen recordings with the client-owned ID", async () => {
    const fetchMock = vi.fn(async () => {
      return new Response(
        JSON.stringify({ ok: true, recordingId: "recording-id" }),
        {
          headers: { "content-type": "application/json" },
          status: 200,
        },
      );
    });
    vi.stubGlobal("fetch", fetchMock);

    await startSimulatorScreenRecording("simulator-id", "recording-id");

    expect(fetchMock).toHaveBeenCalledWith(
      "/api/simulators/simulator-id/screen-recording/start",
      expect.objectContaining({
        body: JSON.stringify({ recordingId: "recording-id" }),
        method: "POST",
      }),
    );
  });
});
