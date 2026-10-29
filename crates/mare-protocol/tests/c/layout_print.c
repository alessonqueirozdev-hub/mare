/* Prints the layout mare.h computes for each configuration on the command
 * line, "inputs,outputs,max_frames,events_in,events_out", one line each:
 * "control audio stride events_in events_out size", or "invalid".
 * Run by tests/c_layout.rs, which compares the lines with mare-protocol. */

#include <stdio.h>
#include <stdlib.h>

#include "mare.h"

int main(int argc, char **argv)
{
    int i;
    for (i = 1; i < argc; i++) {
        unsigned long v[5];
        mare_segment_layout layout;
        if (sscanf(argv[i], "%lu,%lu,%lu,%lu,%lu", &v[0], &v[1], &v[2], &v[3], &v[4]) != 5) {
            fprintf(stderr, "bad argument: %s\n", argv[i]);
            return 2;
        }
        if (mare_segment_layout_of((uint32_t)v[0], (uint32_t)v[1], (uint32_t)v[2],
                                   (uint32_t)v[3], (uint32_t)v[4], &layout) != 0) {
            printf("invalid\n");
            continue;
        }
        printf("%llu %llu %llu %llu %llu %llu\n",
               (unsigned long long)layout.control_offset,
               (unsigned long long)layout.audio_offset,
               (unsigned long long)layout.channel_stride,
               (unsigned long long)layout.events_in_offset,
               (unsigned long long)layout.events_out_offset,
               (unsigned long long)layout.size);
    }
    printf("registry %llu %llu\n", (unsigned long long)mare_registry_size(1),
           (unsigned long long)mare_registry_size(1024));
    printf("ump %u %u %u %u\n", mare_ump_packet_words(0x00000000u),
           mare_ump_packet_words(0x40000000u), mare_ump_packet_words(0xB0000000u),
           mare_ump_packet_words(0xF0000000u));
    return 0;
}
