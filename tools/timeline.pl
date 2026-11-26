# SPDX-License-Identifier: GPL-3.0-or-later
#
# The frame's timeline by draws per frame, from a log of victory.exe run
# with VICTORY_FRAME_WHY2=1.
#
# Usage: perl tools/timeline.pl <log file>
#
# Pairs each "why2 timeline" line with the five-second frame line that
# follows it, skips lines with host pipelines still being made, and prints
# for each band of draws per frame the medians of: the submit worker's last
# execute of title lists, when the relay saw its fence, the tail between
# the two, the present's return (all ms from the present call), and the
# frame, preparing and GPU times. The timeline is the relay's approximate
# observations, not an exact attribution of the frame.
use strict;
use warnings;

sub med { my @s = sort { $a <=> $b } @_; @s ? $s[int($#s / 2)] : 0 }
my $f = shift or die "usage: perl tools/timeline.pl <log file>\n";
open my $in, '<', $f or die "$f: $!\n";
my (%b, @t);
while (<$in>) {
    if (/why2 timeline.*?mean over \d+ frames: title_last_enqueue=\S+ worker_last_execute=(-?[\d.]+) its_fence_seen=(-?[\d.]+) its_landing_end=(-?[\d.]+) present_return=(-?[\d.]+)/) {
        @t = ($1, $2, $4);
    }
    if (/([\d.]+) ms\/frame \(([\d.]+) preparing.*?([\d.]+) on the GPU.*?(\d+) draws\/frame.*?\((\d+) being made/ && @t) {
        my ($ms, $p, $g, $d, $mk) = ($1, $2, $3, $4, $5);
        if (!$mk) {
            my $k = $d < 3000 ? "1-3k" : $d < 7000 ? "3-7k" : $d < 9000 ? "7-9k" : "9k+";
            push @{ $b{$k} }, [@t, $ms, $g, $t[1] - $t[0], $p];
        }
        @t = ();
    }
}
for my $k ("1-3k", "3-7k", "7-9k", "9k+") {
    my $r = $b{$k} or next;
    printf "WHY2 %-5s n=%-2d last_execute %.0f fence %.0f tail %.0f present_return %.0f frame %.0f prep %.0f gpu %.0f\n",
        $k, scalar @$r, (map { my $i = $_; med(map { $_->[$i] } @$r) } 0, 1, 5, 2, 3, 6, 4);
}
