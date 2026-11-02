// The C test client again, in C++ through mare_client.hpp: the same checks,
// the same output, a lambda for the callback and an exception for the
// missing hub.

#include <cstdio>
#include <cstdlib>

#include "mare_client.hpp"

int main(int argc, char** argv) {
    std::printf("sizes %u %u %u\n", unsigned(sizeof(mare_client_config)), unsigned(sizeof(mare_hub_info)),
                unsigned(sizeof(mare_event)));
    if (argc != 3) {
        std::fprintf(stderr, "usage: client HUB MISSING-HUB\n");
        return 2;
    }
    mare::Config config;
    config.name = "c++ client";
    config.input_channels = 2;
    config.output_channels = 2;
    config.connect_timeout = std::chrono::milliseconds(5000);

    try {
        mare::Client::connect(config, argv[2]);
        std::printf("missing connected\n");
    } catch (const mare::Error& error) {
        std::printf("missing %d null\n", error.status());
    }
    try {
        mare::Client::connect(config, "not a hub name/");
    } catch (const mare::Error& error) {
        std::printf("bad name %d\n", error.status());
    }

    mare::Client client = [&] {
        try {
            return mare::Client::connect(config, argv[1]);
        } catch (const mare::Error& error) {
            std::fprintf(stderr, "connect: %s\n", error.what());
            std::exit(4);
        }
    }();
    mare_hub_info info = client.info();
    std::printf("hub %u %u %u %u %u\n", info.sample_rate, info.period_frames, info.max_frames,
                info.device_inputs, info.device_outputs);
    mare_latency latency = client.latency();
    std::printf("latency %u %u %u\n", latency.input_frames, latency.output_frames, latency.source);

    unsigned long cycles = 0, wrong = 0, sent = 0;
    auto process = [&](mare::Cycle& cycle) {
        std::uint32_t frames = cycle.frames();
        if (frames == 0 || frames > info.max_frames || cycle.input_channels() != 2 ||
            cycle.output_channels() != 2 || cycle.input(2) != nullptr || cycle.output(2) != nullptr ||
            cycle.number() == 0) {
            ++wrong;
            return;
        }
        for (std::uint32_t channel = 0; channel < 2; ++channel) {
            const float* in = cycle.input(channel);
            float* out = cycle.output(channel);
            float expected = channel == 0 ? 0.25f : -0.5f;
            for (std::uint32_t i = 0; i < frames; ++i) {
                if (in[i] != expected || out[i] != 0.0f) {
                    ++wrong;
                }
                out[i] = 2.0f * in[i];
            }
        }
        if (sent < 16) {
            mare_event event{cycle.frame_time(), {0x20903C7Fu, 0, 0, 0}};
            sent += cycle.send_event(event) ? 1 : 0;
        }
        ++cycles;
    };

    mare::Result result;
    {
        mare::RealtimeScope realtime;
        do {
            result = client.wait_cycle(std::chrono::milliseconds(100), process);
        } while (result == mare::Result::Ran || result == mare::Result::NoCycle);
    }
    int status = result == mare::Result::HubStopped ? MARE_HUB_STOPPED : MARE_HUB_GONE;
    std::printf("stopped %d cycles %lu wrong %lu sent %lu\n", status, cycles, wrong, sent);
    std::fflush(stdout);
    return status == MARE_HUB_STOPPED && cycles > 0 && wrong == 0 && sent == 16 ? 0 : 1;
}
