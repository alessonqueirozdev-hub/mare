// MARÉ client library, for C++17 and later: a thin header-only layer over
// mare_client.h.
//
// SPDX-License-Identifier: MIT OR Apache-2.0
//
//     auto client = mare::Client::connect({"organ", 0, 2});
//     client.run([&](mare::Cycle& cycle) {
//         float* left = cycle.output(0);
//         ...
//     });
//
// Connecting throws mare::Error on failure; nothing on the cycle path
// throws or allocates. A callback that throws ends the program
// (std::terminate), since an exception must not cross the C library: catch
// inside the callback if it can throw.

#ifndef MARE_CLIENT_HPP
#define MARE_CLIENT_HPP

#include <chrono>
#include <cstdint>
#include <memory>
#include <stdexcept>
#include <string>
#include <type_traits>
#include <utility>

#include "mare_client.h"

namespace mare {

/// A call failed with a mare_client.h status.
class Error : public std::runtime_error {
public:
    explicit Error(int status) : std::runtime_error(mare_status_text(status)), status_(status) {}

    /// The status (MARE_NO_HUB, MARE_FULL, ...).
    int status() const noexcept { return status_; }

private:
    int status_;
};

/// Throws if the library's interface is another major version than this
/// header's.
inline void check_abi() {
    if (MARE_CLIENT_ABI_MAJOR(mare_abi_version()) != MARE_CLIENT_ABI_MAJOR(MARE_CLIENT_ABI_VERSION)) {
        throw std::runtime_error("the MARÉ library is another version than its header");
    }
}

/// What a client asks of the hub.
struct Config {
    std::string name;
    std::uint32_t input_channels = 0;
    std::uint32_t output_channels = 0;
    std::uint32_t events_in_capacity = 256;
    std::uint32_t events_out_capacity = 256;
    std::uint32_t latency_frames = 0;
    std::chrono::milliseconds connect_timeout{2000};
};

/// One cycle, inside a callback. Its pointers are valid until it returns.
class Cycle {
public:
    explicit Cycle(mare_cycle* cycle) noexcept : cycle_(cycle) {}

    std::uint64_t number() const noexcept { return mare_cycle_number(cycle_); }
    std::uint64_t frame_time() const noexcept { return mare_cycle_frame_time(cycle_); }
    std::uint32_t frames() const noexcept { return mare_cycle_frames(cycle_); }
    std::uint32_t input_channels() const noexcept { return mare_cycle_input_channels(cycle_); }
    std::uint32_t output_channels() const noexcept { return mare_cycle_output_channels(cycle_); }

    /// frames() samples, or nullptr if there is no such channel.
    const float* input(std::uint32_t channel) const noexcept { return mare_cycle_input(cycle_, channel); }

    /// frames() samples, zero until written, or nullptr if there is no
    /// such channel.
    float* output(std::uint32_t channel) noexcept { return mare_cycle_output(cycle_, channel); }

    /// The next event from the hub, if there is one.
    bool next_event(mare_event& event) noexcept { return mare_cycle_next_event(cycle_, &event) != 0; }

    /// Queues an event for the hub; false if the ring is full.
    bool send_event(const mare_event& event) noexcept { return mare_cycle_send_event(cycle_, &event) != 0; }

    mare_cycle* get() noexcept { return cycle_; }

private:
    mare_cycle* cycle_;
};

/// How a cycle call ended.
enum class Result {
    Ran,        ///< the cycle ran
    NoCycle,    ///< none was pending, or none came in time
    HubStopped, ///< the hub stopped, or dropped this client
    HubGone,    ///< the hub's process ended
};

namespace detail {

inline Result result(int status) noexcept {
    switch (status) {
    case MARE_OK: return Result::Ran;
    case MARE_NO_CYCLE: return Result::NoCycle;
    case MARE_HUB_GONE: return Result::HubGone;
    default: return Result::HubStopped;
    }
}

template <class F>
void trampoline(mare_cycle* cycle, void* user) noexcept {
    Cycle wrapped(cycle);
    (*static_cast<F*>(user))(wrapped);
}

template <class F>
void* user_of(F& process) noexcept {
    return const_cast<void*>(static_cast<const void*>(std::addressof(process)));
}

} // namespace detail

/// A thread raised to real-time priority for as long as this lives, on the
/// thread that made it.
class RealtimeScope {
public:
    RealtimeScope() noexcept : realtime_(mare_promote_current_thread()) {}
    ~RealtimeScope() { mare_realtime_release(realtime_); }
    RealtimeScope(const RealtimeScope&) = delete;
    RealtimeScope& operator=(const RealtimeScope&) = delete;

    /// Whether the system raised the priority.
    bool promoted() const noexcept { return mare_realtime_is_promoted(realtime_) != 0; }

private:
    mare_realtime* realtime_;
};

/// A client connected to a hub; it leaves when destroyed.
class Client {
public:
    /// Connects to the hub called `hub` (nullptr: the default hub).
    static Client connect(const Config& config, const char* hub = nullptr) {
        check_abi();
        mare_client_config c;
        mare_client_config_init(&c);
        c.name = config.name.c_str();
        c.input_channels = config.input_channels;
        c.output_channels = config.output_channels;
        c.events_in_capacity = config.events_in_capacity;
        c.events_out_capacity = config.events_out_capacity;
        c.latency_frames = config.latency_frames;
        c.connect_timeout_ms = static_cast<std::uint32_t>(config.connect_timeout.count());
        mare_client* client = nullptr;
        int status = mare_connect(hub, &c, &client);
        if (status != MARE_OK) {
            throw Error(status);
        }
        return Client(client);
    }

    Client(Client&& other) noexcept : client_(std::exchange(other.client_, nullptr)) {}
    Client& operator=(Client&& other) noexcept {
        if (this != &other) {
            mare_leave(client_);
            client_ = std::exchange(other.client_, nullptr);
        }
        return *this;
    }
    Client(const Client&) = delete;
    Client& operator=(const Client&) = delete;
    ~Client() { mare_leave(client_); }

    /// How the hub runs.
    mare_hub_info info() const noexcept {
        mare_hub_info info{};
        mare_client_info(client_, &info);
        return info;
    }

    // The hub's latency now; it may change while the hub runs.
    mare_latency latency() const noexcept {
        mare_latency latency{};
        mare_client_latency(client_, &latency);
        return latency;
    }

    std::uint64_t last_cycle() const noexcept { return mare_client_last_cycle(client_); }
    std::uint64_t late_cycles() const noexcept { return mare_client_late_cycles(client_); }

    /// Waits up to `timeout` for a cycle and runs `process(Cycle&)` on it.
    template <class F>
    Result wait_cycle(std::chrono::milliseconds timeout, F&& process) noexcept {
        using Fn = std::remove_reference_t<F>;
        int status = mare_wait_cycle(client_, static_cast<std::uint32_t>(timeout.count()),
                                     &detail::trampoline<Fn>, detail::user_of(process));
        return detail::result(status);
    }

    /// Runs the pending cycle, if any, without waiting.
    template <class F>
    Result try_cycle(F&& process) noexcept {
        using Fn = std::remove_reference_t<F>;
        return detail::result(mare_try_cycle(client_, &detail::trampoline<Fn>, detail::user_of(process)));
    }

    /// Runs a cycle each period, on this thread raised to real-time
    /// priority, until the hub stops or goes away.
    template <class F>
    Result run(F&& process) noexcept {
        using Fn = std::remove_reference_t<F>;
        return detail::result(mare_run(client_, &detail::trampoline<Fn>, detail::user_of(process)));
    }

    mare_client* get() noexcept { return client_; }

private:
    explicit Client(mare_client* client) noexcept : client_(client) {}
    mare_client* client_;
};

} // namespace mare

#endif // MARE_CLIENT_HPP
