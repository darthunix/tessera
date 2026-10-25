#!/bin/sh
# The check of the rule "One way to memory" of the contract of relhash:
# the list of the places where the table trusts its own bytes is
# complete (openspec/specs/relhash/design.md, "Where the table trusts
# its bytes").
#   check-trust.sh
# Every method of the region, the one layer of memory of a table, is
# known to this script as a read of the table's blocks or not. Every
# call of a read in the crate's sources carries a mark
# `// trust: <name>`, at the end of its line or in the comments right
# above it, and every mark is on such a call. Every name is a trust
# point of the design, and every point is marked somewhere. The region
# itself, the loom models and the tests are not searched.
# Run from the root of the repository.
set -eu

crate=crates/relhash/src
region=$crate/table/region.rs
design=openspec/specs/relhash/design.md

git ls-files -- "$crate/*.rs" |
    grep -v -E "^$region\$|^$crate/table/(loom/|tests/|tests\.rs\$|testing\.rs\$)" |
    perl -e '
        my ($region, $design) = @ARGV[0, 1];
        my @files = <STDIN>;
        chomp @files;
        my $fail = 0;
        sub error { print STDERR "check-trust: @_\n"; $fail = 1 }

        # The methods of the region: those that read the index or a
        # chunk, and the rest. A new method is put in one of the two.
        my %reads = map { ($_ => 1) } qw(load_u32_relaxed load_u64_relaxed
            load_u64 fetch_add_u64 load_u32_in cas_u32_in chunk_used
            chunk_bytes load_next);
        my %others = map { ($_ => 1) } qw(len store_u32 store_u64 zero_u32
            prefetch chunks chunk_len chunk_fits set_chunk_used spot advance
            chunk_bytes_mut store_next prefetch_record);
        open(my $in, "<", $region) or die "$region: $!";
        my $source = do { local $/; <$in> };
        my ($trait) = $source =~ /\btrait Region \{(.*?)\n\}/s
            or die "$region: no trait Region";
        my %methods = map { ($_ => 1) } $trait =~ /\bfn (\w+)/g;
        for my $method (sort keys %methods) {
            error("$region: the method $method"
                . " is neither a read nor another method of check-trust.sh")
                unless $reads{$method} || $others{$method};
        }
        for my $method (sort keys %reads, sort keys %others) {
            error("$region: check-trust.sh names $method, which the region"
                . " does not have") unless $methods{$method};
        }
        my $read = join "|", sort keys %reads;

        # The trust points: the items of their section of the design.
        open($in, "<", $design) or die "$design: $!";
        my (%points, %marked, $in_section);
        while (<$in>) {
            $in_section = /^## Where the table trusts its bytes$/
                if /^## /;
            $points{$1} = $. if $in_section && /^- \*\*`([a-z][a-z-]*)`\*\*/;
        }
        error("$design: no trust points") unless %points;

        for my $file (@files) {
            open($in, "<", $file) or die "$file: $!";
            my @lines = <$in>;
            for my $at (0 .. $#lines) {
                # The module of the tests closes a file.
                last if $lines[$at] =~ /^#\[cfg\(test\)\]$/
                    && ($lines[$at + 1] // "") =~ /^mod /;
                my $line = $lines[$at];
                my $where = "$file:" . ($at + 1);
                my $mark = $line =~ m{//\s*trust:\s*(\S+)\s*$} ? $1 : undef;
                my $code = $line =~ m{^\s*//} ? "" : $line =~ s{//.*}{}r;
                if ($code !~ /\.(?:$read)\s*\(/) {
                    next unless defined $mark;
                    # A mark above a call: the next line that is not a
                    # comment is the call.
                    my $next = $at + 1;
                    $next++ while $next <= $#lines
                        && $lines[$next] =~ m{^\s*//};
                    my $after = ($lines[$next] // "") =~ s{//.*}{}r;
                    error("$where: a trust mark on no read of the region")
                        unless $after =~ /\.(?:$read)\s*\(/;
                    next;
                }
                # A read: its mark is on its line or in the comments above.
                my $above = $at - 1;
                while (!defined $mark && $above >= 0
                    && $lines[$above] =~ m{^\s*//}) {
                    $mark = $1 if $lines[$above] =~ m{//\s*trust:\s*(\S+)\s*$};
                    $above--;
                }
                if (!defined $mark) {
                    error("$where: a read of the region without a trust mark");
                } elsif (!$points{$mark}) {
                    error("$where: the trust point $mark is not in $design");
                } else {
                    $marked{$mark} = 1;
                }
            }
        }
        for my $point (sort keys %points) {
            error("$design:$points{$point}: the trust point $point is"
                . " marked nowhere") unless $marked{$point};
        }
        exit $fail;
    ' "$region" "$design"
