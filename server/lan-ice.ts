import { isLocalAddress } from "./network.js";
import { GatewayError } from "../shared/contracts.js";
import type { RTCPeerConnection } from "werift";

/** werift's underlying ICE layer otherwise installs its public STUN default. */
export function localIceOnly(peer: RTCPeerConnection) {
  for (const transport of peer.iceTransports)
    transport.connection.stunServer = undefined;
}
export function checkIceCandidate(candidate: string) {
  const address = candidate.trim().split(/\s+/)[4];
  if (!address || !isLocalAddress(address))
    throw new GatewayError(
      "INVALID_ADDRESS",
      "ICE must use local/LAN addresses",
    );
}
export function checkIceDescription(sdp: string) {
  for (const line of sdp.split(/\r?\n/))
    if (line.startsWith("a=candidate:")) checkIceCandidate(line.slice(2));
}
