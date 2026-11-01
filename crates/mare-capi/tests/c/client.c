/*
 * A client in C, run by tests/c_client.rs against a hub whose device records
 * 0.25 on channel 0 and -0.5 on channel 1.
 *
 * It checks what a missing hub and a bad name return, connects to the hub
 * named on its command line, and runs cycles until the hub stops: each
 * checks its inputs, writes twice its inputs to its outputs, and the first
 * sixteen send an event. It prints the sizes of its structures and what it
 * saw, and exits 0 if everything was as expected.
 */

#include <stdio.h>

#include "mare_client.h"

typedef struct state {
    uint32_t max_frames;
    unsigned long cycles;
    unsigned long wrong;
    unsigned long sent;
} state;

static void process(mare_cycle *cycle, void *user) {
    state *s = (state *)user;
    uint32_t frames = mare_cycle_frames(cycle);
    uint32_t channel;
    uint32_t i;
    if (frames == 0 || frames > s->max_frames || mare_cycle_input_channels(cycle) != 2 ||
        mare_cycle_output_channels(cycle) != 2 || mare_cycle_input(cycle, 2) != NULL ||
        mare_cycle_output(cycle, 2) != NULL || mare_cycle_number(cycle) == 0) {
        s->wrong++;
        return;
    }
    for (channel = 0; channel < 2; channel++) {
        const float *in = mare_cycle_input(cycle, channel);
        float *out = mare_cycle_output(cycle, channel);
        float expected = channel == 0 ? 0.25f : -0.5f;
        for (i = 0; i < frames; i++) {
            if (in[i] != expected || out[i] != 0.0f) {
                s->wrong++;
            }
            out[i] = 2.0f * in[i];
        }
    }
    if (s->sent < 16) {
        mare_event event = {0, {0x20903C7Fu, 0, 0, 0}};
        event.time = mare_cycle_frame_time(cycle);
        s->sent += (unsigned long)mare_cycle_send_event(cycle, &event);
    }
    s->cycles++;
}

int main(int argc, char **argv) {
    mare_client_config config;
    mare_client *client = NULL;
    mare_hub_info info;
    mare_realtime *realtime;
    state s = {0, 0, 0, 0};
    int status;

    printf("sizes %u %u %u\n", (unsigned)sizeof(mare_client_config), (unsigned)sizeof(mare_hub_info),
           (unsigned)sizeof(mare_event));
    if (argc != 3) {
        fprintf(stderr, "usage: client HUB MISSING-HUB\n");
        return 2;
    }
    if (MARE_CLIENT_ABI_MAJOR(mare_abi_version()) != MARE_CLIENT_ABI_MAJOR(MARE_CLIENT_ABI_VERSION)) {
        fprintf(stderr, "library version %x\n", mare_abi_version());
        return 3;
    }

    mare_client_config_init(&config);
    config.name = "c client";
    config.input_channels = 2;
    config.output_channels = 2;
    config.connect_timeout_ms = 5000;

    status = mare_connect(argv[2], &config, &client);
    printf("missing %d %s\n", status, client == NULL ? "null" : "set");
    status = mare_connect("not a hub name/", &config, &client);
    printf("bad name %d\n", status);

    status = mare_connect(argv[1], &config, &client);
    if (status != MARE_OK) {
        fprintf(stderr, "connect: %s\n", mare_status_text(status));
        return 4;
    }
    mare_client_info(client, &info);
    printf("hub %u %u %u\n", info.sample_rate, info.period_frames, info.max_frames);
    s.max_frames = info.max_frames;

    realtime = mare_promote_current_thread();
    do {
        status = mare_wait_cycle(client, 100, process, &s);
    } while (status == MARE_OK || status == MARE_NO_CYCLE);
    mare_realtime_release(realtime);

    printf("stopped %d cycles %lu wrong %lu sent %lu\n", status, s.cycles, s.wrong, s.sent);
    fflush(stdout);
    mare_leave(client);
    return status == MARE_HUB_STOPPED && s.cycles > 0 && s.wrong == 0 && s.sent == 16 ? 0 : 1;
}
