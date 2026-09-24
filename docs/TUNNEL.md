# Reaching a device's own web page

An installer standing at a site can open the NVR's settings page. Nobody
outside can, short of forwarding a port or running a VPN — which is how sites
end up with an NVR on the public internet, and why the first thing anyone asks
for is a way to reach it without that.

The gateway already holds an outbound connection. This carries one HTTP
request at a time through it: the control plane hands over a request, the
gateway performs it on the camera network, and the answer comes back.

## Using it

*Device page* on a camera's card opens a tunnel and a new tab. The address
comes from what that camera reported — there is no box to type one into, on
purpose.

```text
POST /api/v1/gateways/{gateway_id}/tunnel   {"host": "10.0.0.7", "port": 80, "minutes": 10}
GET  /api/v1/tunnels/{session_id}/{path}
POST /api/v1/tunnels/{session_id}/close
```

## The limits are the feature

| | |
| --- | --- |
| **Only a device the gateway reported** | a host that has never turned up as a camera or a source is refused. Anything looser is a proxy into somebody's network wearing a camera's name |
| **Technician or owner** | never a customer-scoped login, which is checked twice: the route's group, and again in the handler |
| **Off unless the site says otherwise** | `TUNNEL_ENABLED=true` on the gateway. A site that says nothing cannot be tunnelled into by anybody, including a control plane somebody else has taken over |
| **Time-boxed** | ten minutes by default, an hour at most, and an expired session is forgotten rather than slow |
| **Four megabytes** | an answer over that is refused, not truncated: half a page is a broken page presented as a page |
| **Audited** | who opened it, onto what, for how long, and on closing how many requests it carried |

## What it is not

- **Not a VPN.** One host, one port, GET only, one request at a time. No
  websockets, no upgrades, no streaming, no POST — a settings page you can
  read is the goal; changing settings through it is not offered yet.
- **Not a video path.** Live view and recordings have their own way out, which
  does not go through the control plane's bandwidth. This deliberately does.
- **Not a login.** The device asks for its own password and the browser
  answers it. The gateway has camera credentials and does not spend them here:
  a tunnel that logs you in is a tunnel that logs anybody in.
- **Not fast.** Every request is a round trip through the control plane and
  back. A settings page loads; a device UI that pulls three hundred assets
  will feel like it.

## What has never been tried

No real NVR has been opened through this. It is tested against a fake device
in the gateway's own test suite and a fake gateway in the API's, which proves
the plumbing and says nothing about what a real device's web UI does when its
assets arrive one at a time through somebody else's request queue.

Nor has anybody left one open on a site network for a month. The thing to
watch is the audit log: every session says who, onto what, and how much it
carried.
