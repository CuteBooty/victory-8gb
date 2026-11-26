# SPDX-License-Identifier: GPL-3.0-or-later
#
# Frame times by draws per frame, from a log of victory.exe.
#
# Usage: perl tools/bands.pl <log file>
#
# Reads the five-second lines ("... fps, ... ms/frame (... preparing ...
# on the GPU ...), ... draws/frame ... (N being made) ..."), skips lines
# with fewer than 1,000 draws or with host pipelines still being made, and
# prints for each band of draws per frame: the number of lines and the
# medians of ms/frame, of the preparing time and of the GPU time.
use strict;
use warnings;

my %b;
while (<>) {
    next unless / fps, /;
    next unless /([\d.]+) ms\/frame \(([\d.]+) preparing.*?([\d.]+) on the GPU.*?(\d+) draws\/frame.*?\((\d+) being made/;
    next if $5 > 0;
    my ($ms, $p, $g, $d) = ($1, $2, $3, $4);
    next if $d < 1000;
    my $k = $d < 3000 ? "1-3k" : $d < 5000 ? "3-5k" : $d < 7000 ? "5-7k" : $d < 9000 ? "7-9k" : $d < 11000 ? "9-11k" : ">11k";
    push @{ $b{$k}{ms} }, $ms;
    push @{ $b{$k}{p} },  $p;
    push @{ $b{$k}{g} },  $g;
}
sub med { my @s = sort { $a <=> $b } @_; $s[int($#s / 2)] }
for my $k ("1-3k", "3-5k", "5-7k", "7-9k", "9-11k", ">11k") {
    next unless $b{$k};
    printf "%-6s n=%-3d ms %.0f  prep %.0f  gpu %.0f\n", $k, scalar @{ $b{$k}{ms} },
        med(@{ $b{$k}{ms} }), med(@{ $b{$k}{p} }), med(@{ $b{$k}{g} });
}
