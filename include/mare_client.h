/*
 * MARÉ client library, for C and C++.
 *
 * SPDX-License-Identifier: MIT OR Apache-2.0
 *
 * Link against `mareclient` (mareclient.dll.lib and mareclient.dll on
 * Windows, libmareclient.so on Linux, libmareclient.dylib on macOS), built
 * by the `mare-capi` crate.
 *
 * A client connects to a running hub by name, then runs one cycle each
 * period from a callback: it reads the hub's audio from its inputs, writes
 * its own to its outputs, and may send and receive events (MIDI 2.0
 * Universal MIDI Packets, each stamped with the frame it belongs to).
 *
 *     mare_client_config config;
 *     mare_client_config_init(&config);
 *     config.name = "organ";
 *     config.output_channels = 2;
 *     mare_client *client;
 *     if (mare_connect(NULL, &config, &client) != MARE_OK) { ... }
 *     mare_run(client, process, state);   // until the hub stops
 *     mare_leave(client);
 *
 * The callback runs on a real-time thread: it must not allocate, lock,
 * wait or do I/O, and must finish well within the period. Everything the
 * library does on that path (mare_wait_cycle, mare_try_cycle, mare_run and
 * the mare_cycle_* functions) allocates nothing and takes no lock.
 *
 * A client is used by one thread at a time; it may move between threads.
 * A callback must not unwind (C++ callbacks catch their exceptions).
 */

#ifndef MARE_CLIENT_H
#define MARE_CLIENT_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* The version of this interface: major in the high 16 bits, minor in the
 * low. A library whose major version differs must not be used. */
#define MARE_CLIENT_ABI_VERSION 0x00000002u
#define MARE_CLIENT_ABI_MAJOR(v) ((v) >> 16)

/* What every call returns. */
enum {
    MARE_OK = 0,              /* done; for a cycle call, the cycle ran */
    MARE_NO_HUB = 1,          /* no hub of that name is running */
    MARE_HUB_NOT_RUNNING = 2, /* the hub is starting or stopping */
    MARE_BAD_REGISTRY = 3,    /* the hub is another version of MARÉ */
    MARE_BAD_CONFIG = 4,      /* the configuration cannot be laid out */
    MARE_FULL = 5,            /* the hub has no free slot */
    MARE_REJECTED = 6,        /* the hub refused the client */
    MARE_TIMEOUT = 7,         /* the hub did not answer in time */
    MARE_IO = 8,              /* the operating system refused something */
    MARE_HUB_STOPPED = 9,     /* the hub stopped, or dropped this client */
    MARE_HUB_GONE = 10,       /* the hub's process ended */
    MARE_NO_CYCLE = 11,       /* no cycle was pending, or none came in time */
    MARE_BAD_ARGUMENT = 12    /* null, not UTF-8, or out of range */
};

typedef struct mare_client mare_client;
typedef struct mare_cycle mare_cycle;
typedef struct mare_realtime mare_realtime;

/* What a client asks of the hub. Fill it with mare_client_config_init,
 * then change what you need. */
typedef struct mare_client_config {
    uint32_t size;                /* set by mare_client_config_init */
    uint32_t input_channels;      /* audio channels into the client */
    uint32_t output_channels;     /* audio channels out of the client */
    uint32_t events_in_capacity;  /* 0 or a power of two (default 256) */
    uint32_t events_out_capacity; /* 0 or a power of two (default 256) */
    uint32_t latency_frames;      /* latency the client adds (default 0) */
    uint32_t connect_timeout_ms;  /* default 2000 */
    uint32_t reserved;            /* 0 */
    const char *name;             /* UTF-8, at most 64 bytes, may be NULL */
} mare_client_config;

/* How the hub runs. */
typedef struct mare_hub_info {
    uint32_t sample_rate;   /* frames per second */
    uint32_t period_frames; /* frames in each cycle */
    uint32_t max_frames;    /* the longest cycle */
    uint32_t hub_pid;       /* the hub's process id */
    uint32_t device_inputs;  /* channels the device records (0: unknown) */
    uint32_t device_outputs; /* channels the device plays (0: unknown) */
    uint32_t reserved;       /* 0 */
} mare_hub_info;

/* Where a hub's latency comes from (the same values as mare.h's). */
#ifndef MARE_LATENCY_UNKNOWN
#define MARE_LATENCY_UNKNOWN 0u  /* nobody has said; the latencies are 0 */
#define MARE_LATENCY_REPORTED 1u /* worked out from the driver's figures */
#define MARE_LATENCY_MEASURED 2u /* measured, and given to the hub */
#endif

/* The hub's latency, in frames (since ABI 0.2). */
typedef struct mare_latency {
    uint32_t input_frames;  /* a cycle's first input frame entering the device to the cycle's start (0: unknown) */
    uint32_t output_frames; /* a cycle's start to its first output frame leaving the device (0: unknown) */
    uint32_t source;        /* MARE_LATENCY_* */
    uint32_t reserved;      /* 0 */
} mare_latency;

/* A Universal MIDI Packet of up to four words, and the frame (on the hub's
 * clock) it belongs to. */
typedef struct mare_event {
    uint64_t time;
    uint32_t words[4];
} mare_event;

/* Runs one cycle. `cycle` is valid only until it returns. */
typedef void (*mare_process_fn)(mare_cycle *cycle, void *user);

/* The library's MARE_CLIENT_ABI_VERSION. */
uint32_t mare_abi_version(void);

/* A short English description of a status, never NULL. */
const char *mare_status_text(int status);

/* Fills `config` with the defaults. */
void mare_client_config_init(mare_client_config *config);

/* Connects to the hub called `hub` (NULL: the default hub). On success
 * stores the client in `*client`; on failure stores NULL. */
int mare_connect(const char *hub, const mare_client_config *config, mare_client **client);

/* Leaves the hub and frees the client. NULL is ignored. */
void mare_leave(mare_client *client);

/* How the hub runs. */
int mare_client_info(const mare_client *client, mare_hub_info *info);

/* Stores the hub's latency now in `*latency`. The hub may change it while it
 * runs; call this off the cycle, from time to time, to follow it (since ABI
 * 0.2). */
int mare_client_latency(const mare_client *client, mare_latency *latency);

/* The last cycle this client finished (0 before the first). */
uint64_t mare_client_last_cycle(const mare_client *client);

/* Cycles the hub says this client missed. */
uint64_t mare_client_late_cycles(const mare_client *client);

/* Waits up to `timeout_ms` for a cycle and runs `process(cycle, user)` on
 * it. Returns MARE_OK if it ran, MARE_NO_CYCLE if none came in time,
 * MARE_HUB_STOPPED or MARE_HUB_GONE. The hub wakes a client only once it
 * has first called this, mare_try_cycle or mare_run. A thread that loops on
 * this should raise itself with mare_promote_current_thread. */
int mare_wait_cycle(mare_client *client, uint32_t timeout_ms, mare_process_fn process, void *user);

/* Runs the pending cycle, if any, without waiting: MARE_OK if it ran,
 * MARE_NO_CYCLE if none was pending, or MARE_HUB_STOPPED. */
int mare_try_cycle(mare_client *client, mare_process_fn process, void *user);

/* Runs a cycle each period, on this thread raised to real-time priority,
 * until the hub stops or goes away: returns MARE_HUB_STOPPED or
 * MARE_HUB_GONE. To stop on your own, loop on mare_wait_cycle instead. */
int mare_run(mare_client *client, mare_process_fn process, void *user);

/* Raises the calling thread to real-time priority. Returns a handle to
 * release on the same thread, or NULL if the system could not be asked. */
mare_realtime *mare_promote_current_thread(void);

/* 1 if the system raised the thread's priority, else 0. */
int mare_realtime_is_promoted(const mare_realtime *realtime);

/* Gives the priority back. NULL is ignored. */
void mare_realtime_release(mare_realtime *realtime);

/* Inside a callback. */
uint64_t mare_cycle_number(const mare_cycle *cycle);     /* from 1 */
uint64_t mare_cycle_frame_time(const mare_cycle *cycle); /* first frame */
uint32_t mare_cycle_frames(const mare_cycle *cycle);
uint32_t mare_cycle_input_channels(const mare_cycle *cycle);
uint32_t mare_cycle_output_channels(const mare_cycle *cycle);

/* mare_cycle_frames samples of a channel, or NULL if there is no such
 * channel. Outputs start at zero. */
const float *mare_cycle_input(const mare_cycle *cycle, uint32_t channel);
float *mare_cycle_output(mare_cycle *cycle, uint32_t channel);

/* The next event from the hub into `*event`: 1 if there was one, else 0. */
int mare_cycle_next_event(mare_cycle *cycle, mare_event *event);

/* Queues an event for the hub: 1 if queued, 0 if the ring is full. */
int mare_cycle_send_event(mare_cycle *cycle, const mare_event *event);

#ifdef __cplusplus
}
#endif

#endif /* MARE_CLIENT_H */
