package org.argus.droid

/**
 * Where the daemon is, from the phone's point of view.
 *
 * The one thing that is genuinely different about the Android client: `localhost`
 * on a phone is the phone. Under the emulator the host loopback is reachable at
 * 10.0.2.2, which is also why this build needs no pairing token yet — the daemon
 * sees those requests arriving on 127.0.0.1 and its `loopback_exempt` policy lets
 * them through. On real hardware over the LAN or Tailscale it will not, and the
 * QR pairing flow the server already implements is what fills that gap.
 */
object Server {
    /** Emulator alias for the machine running the emulator. */
    const val EMULATOR_HOST = "http://10.0.2.2:8787"

    /**
     * Over USB, `adb reverse tcp:8787 tcp:8787` makes the handset's own
     * loopback come out on the development machine's. Worth preferring to a LAN
     * address during development for two reasons: nothing has to be bound to
     * the network, and the daemon sees the request arrive on 127.0.0.1, so its
     * `loopback_exempt` policy applies and no pairing is needed yet.
     */
    const val ADB_REVERSE_HOST = "http://127.0.0.1:8787"

    var baseUrl: String = EMULATOR_HOST

    val styleUrl: String get() = "$baseUrl/v1/style.json"
    val healthUrl: String get() = "$baseUrl/v1/health"
}
