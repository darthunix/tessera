#!/bin/sh
# The checks of two rules of the contract of relhash, against the lists
# of its design (openspec/specs/relhash/design.md):
#   check-trust.sh
# 1. "One way to memory", "Where the table trusts its bytes": every
#    method of the region, the one layer of memory of a table, is known
#    to this script as a read of the table's blocks or not. Every call
#    of a read in the crate's sources carries a mark `// trust: <name>`,
#    at the end of its line or in the comments right above it, and every
#    mark is on such a call. Every name is a trust point of the design,
#    and every point is marked somewhere. The region itself, the loom
#    models and the tests are not searched.
# 2. "Unsafe code where it is proven", "Unsafe code": a module off the
#    design's list starts with `#![forbid(unsafe_code)]`, or is the
#    crate's root and denies it, and allows it nowhere; a module of the
#    list holds unsafe code.
# Run from the root of the repository.
set -eu

crate=crates/relhash/src
design=openspec/specs/relhash/design.md

git ls-files -- "$crate/*.rs" | perl -e '
    my ($crate, $design) = @ARGV[0, 1];
    my $region = "$crate/table/region.rs";
    my @files = <STDIN>;
    chomp @files;
    my $fail = 0;
    sub error { print STDERR "check-trust: @_\n"; $fail = 1 }
    sub slurp { open(my $in, "<", $_[0]) or die "$_[0]: $!"; local $/; <$in> }
    # The code of a line, without its comment.
    sub code { $_[0] =~ m{^\s*//} ? "" : $_[0] =~ s{//.*}{}r }

    # The items of a section of the design: each text in bold backticks,
    # with its line.
    sub items {
        my ($title) = @_;
        open(my $in, "<", $design) or die "$design: $!";
        my ($in_section, %items);
        while (<$in>) {
            $in_section = $_ eq "## $title\n" if /^## /;
            next unless $in_section;
            $items{$1} //= $. while /\*\*`([^`]+)`\*\*/g;
        }
        error("$design: no items under \"$title\"") unless %items;
        return %items;
    }

    # 1. The methods of the region: those that read the index or a
    # chunk, and the rest. A new method is put in one of the two.
    my %reads = map { ($_ => 1) } qw(load_u32_relaxed load_u64_relaxed
        load_u64 fetch_add_u64 load_u32_in cas_u32_in chunk_used
        chunk_bytes load_next);
    my %others = map { ($_ => 1) } qw(len store_u32 store_u64 zero_u32
        prefetch chunks chunk_len chunk_fits set_chunk_used spot advance
        chunk_bytes_mut store_next prefetch_record);
    my ($trait) = slurp($region) =~ /\btrait Region \{(.*?)\n\}/s
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

    my %points = items("Where the table trusts its bytes");
    my %marked;
    for my $file (@files) {
        next if $file eq $region
            || $file =~ m{^\Q$crate\E/table/(loom/|tests/|tests\.rs$|testing\.rs$)};
        open(my $in, "<", $file) or die "$file: $!";
        my @lines = <$in>;
        for my $at (0 .. $#lines) {
            # The module of the tests closes a file.
            last if $lines[$at] =~ /^#\[cfg\(test\)\]$/
                && ($lines[$at + 1] // "") =~ /^mod /;
            my $line = $lines[$at];
            my $where = "$file:" . ($at + 1);
            my $mark = $line =~ m{//\s*trust:\s*(\S+)\s*$} ? $1 : undef;
            if (code($line) !~ /\.(?:$read)\s*\(/) {
                next unless defined $mark;
                # A mark above a call: the next line that is not a
                # comment is the call.
                my $next = $at + 1;
                $next++ while $next <= $#lines && $lines[$next] =~ m{^\s*//};
                error("$where: a trust mark on no read of the region")
                    unless code($lines[$next] // "") =~ /\.(?:$read)\s*\(/;
                next;
            }
            # A read: its mark is on its line or in the comments above.
            my $above = $at - 1;
            while (!defined $mark && $above >= 0 && $lines[$above] =~ m{^\s*//}) {
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

    # 2. The modules that may hold unsafe code; a path that ends in a
    # slash is a folder.
    my %unsafe = items("Unsafe code");
    my %holds;
    for my $file (@files) {
        (my $path = $file) =~ s{^\Q$crate\E/}{};
        my ($listed) = grep { $_ eq $path || (m{/$} && index($path, $_) == 0) }
            keys %unsafe;
        my $source = slurp($file);
        my $code = join "", map { code($_) } split /^/, $source;
        if (defined $listed) {
            $holds{$listed} = 1 if $code =~ /\bunsafe\b/;
            next;
        }
        error("$file: allows unsafe code but is not in $design")
            if $code =~ /\ballow\(\s*unsafe_code\s*\)/;
        if ($path eq "lib.rs") {
            error("$file: the root does not deny unsafe code")
                unless $source =~ /^#!\[deny\(unsafe_code\)\]$/m;
        } else {
            error("$file: does not forbid unsafe code, and is not in $design")
                unless $source =~ /^#!\[forbid\(unsafe_code\)\]$/m;
        }
    }
    for my $path (sort keys %unsafe) {
        error("$design:$unsafe{$path}: $path holds no unsafe code")
            unless $holds{$path};
    }
    exit $fail;
' "$crate" "$design"
