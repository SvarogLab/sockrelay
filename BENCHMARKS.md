# Benchmarks: sockrelay vs. the old socat+npiperelay bridge

Both measured with [`sockrelay-loadtest`](../sockrelay-loadtest), same parameters, same real
Windows OpenSSH agent, back to back, on the same machine — 32 concurrent clients, 15 seconds,
fresh connect-per-request (matching real `ssh`/`ssh-add` behavior).

The old bridge was run on a throwaway socket path alongside the live, already-running `sockrelay`
daemon, so this isn't a cold-start comparison — both were warmed up and otherwise idle.

## Results

|                        | sockrelay | socat + npiperelay |
|------------------------|-----------|---------------------|
| completed requests     | 4,812     | 856                 |
| req/s                  | 316.1     | 55.2                |
| connect p50            | 66.4µs    | 72.3µs              |
| connect p99            | 224µs     | 4.14ms              |
| connect max             | 1.40ms    | 8.02ms              |
| request p50            | 66.6ms    | 346.6ms             |
| request p99            | 482.4ms   | 1.576s              |
| request max            | 801.0ms   | 2.002s              |
| dial errors            | 0         | 1,402               |
| request errors         | 0         | 270 (all "early eof")|

## Raw output

```
$ sockrelay-loadtest --target ~/.ssh/agent.sock --clients 32 --duration-secs 15
sockrelay-loadtest: target="/home/seacat/.ssh/agent.sock" clients=32 duration=15s timeout=10s
completed requests: 4812   wall time: 15.22s   req/s: 316.1

phase           n        min        p50        p90        p99      p99.9        max
connect      4812   13.799µs   66.397µs  106.495µs  224.089µs  499.376µs 1.395032ms
request      4812 8.769376ms 66.603779ms 233.224523ms 482.388174ms 634.782494ms 801.027279ms

dial errors: 0
request errors: 0
```

```
$ sockrelay-loadtest --target /tmp/socat-compare.sock --clients 32 --duration-secs 15
sockrelay-loadtest: target="/tmp/socat-compare.sock" clients=32 duration=15s timeout=10s
completed requests: 856   wall time: 15.50s   req/s: 55.2

phase           n        min        p50        p90        p99      p99.9        max
connect       856   13.399µs   72.296µs  123.894µs   4.1379ms 8.009814ms 8.017513ms
request       856 198.41582ms 346.565968ms 441.995202ms 1.57594766s 1.974944191s 2.002226977s

dial errors: 1402
  e.g. Resource temporarily unavailable (os error 11)
request errors: 270
  e.g. io error: early eof
```

## Root cause, confirmed

The bridge's own log for that run contains **exactly 270** lines of:

```
open //./pipe/openssh-ssh-agent: All pipe instances are busy.
```

— an exact match for the 270 "early eof" request errors above. This is the original bug this whole
project exists to fix, caught in the act: `socat`'s `fork` mode spawns a brand-new `npiperelay.exe`
process for *every single connection*, each of which makes exactly one, non-retried attempt to open
the named pipe. Under concurrent load, most of those fresh processes lose the race for a free pipe
instance, fail immediately, and `socat` just closes the connection — which is exactly what a real
parallel `ssh`/`ansible` invocation looks like from the client's point of view: a `communication
with agent failed` error, with no indication of why.

`sockrelay` never has this failure mode by construction: the Windows-side process is spawned
**once**, and every connection is multiplexed over that single already-running process — there is
no per-connection process spawn to race in the first place. What busy-pipe contention *does* still
exist (the real agent's own multi-request handling) is absorbed by `sockrelay-server`'s
`WaitNamedPipeW`-based wait-and-retry loop (see the main README's Configuration section) rather than
surfacing as a hard failure.
