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
# 3. "One way to bytes read back", "Where the crate trusts its bytes" of
#    relspill: every call of the crate's layer of reads, the module
#    read.rs, carries a mark `// trust: <name>` as in 1, every name is a
#    trust point of the crate's design and every point is marked; every
#    function of the layer is known to this script; no word is read
#    from bytes outside the layer, and no module imports the layer's
#    functions. The tests, the proofs and the loom models are not
#    searched.
# Run from the root of the repository.
set -eu

crate=crates/relhash/src
design=openspec/specs/relhash/design.md
status=0

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
' "$crate" "$design" || status=1

# 3. relspill: its design is the change's until the change is closed.
spill=crates/relspill/src
spill_design=openspec/specs/relspill/design.md
test -f "$spill_design" || spill_design=openspec/changes/spill-crate/design.md
git ls-files -- "$spill/*.rs" | perl -e '
    my ($crate, $design) = @ARGV[0, 1];
    my $layer = "$crate/read.rs";
    my @files = <STDIN>;
    chomp @files;
    my $fail = 0;
    sub error { print STDERR "check-trust: @_\n"; $fail = 1 }
    sub slurp { open(my $in, "<", $_[0]) or die "$_[0]: $!"; local $/; <$in> }
    sub code { $_[0] =~ m{^\s*//} ? "" : $_[0] =~ s{//.*}{}r }

    my %points;
    {
        open(my $in, "<", $design) or die "$design: $!";
        my $in_section;
        while (<$in>) {
            $in_section = $_ eq "## Where the crate trusts its bytes\n" if /^## /;
            next unless $in_section;
            $points{$1} //= $. while /\*\*`([^`]+)`\*\*/g;
        }
        error("$design: no items under \"Where the crate trusts its bytes\"")
            unless %points;
    }

    # The functions of the layer, each a read of bytes not written in
    # the call; a new one is added here.
    my %reads = map { ($_ => 1) } qw(u8_at u32_at u64_at values u32_of
        u64_of entries entry entry_in page_word);
    my %functions = map { ($_ => 1) } slurp($layer) =~ /\bfn (\w+)/g;
    for my $function (sort keys %functions) {
        error("$layer: the function $function is no read check-trust.sh knows")
            unless $reads{$function};
    }
    for my $function (sort keys %reads) {
        error("$layer: check-trust.sh names $function, which the layer"
            . " does not have") unless $functions{$function};
    }
    my $read = join "|", sort keys %reads;

    my %marked;
    for my $file (@files) {
        next if $file eq $layer
            || $file =~ m{^\Q$crate\E/(loom\.rs|testing\.rs|(?:.*/)?proofs\.rs|.*/tests\.rs|join/model\.rs)$};
        open(my $in, "<", $file) or die "$file: $!";
        my @lines = <$in>;
        # A file of tests: its own inner attribute keeps it out of every
        # other build, whichever module names it.
        next if grep { /^#!\[cfg\(test\)\]\s*$/ } @lines;
        my $tests = 0;
        for my $at (0 .. $#lines) {
            # A module of tests, to the brace that closes it at the
            # start of a line; what follows it is searched again.
            if ($lines[$at] =~ /^#\[cfg\(test\)\]$/
                && ($lines[$at + 1] // "") =~ /^(?:pub(?:\(crate\))? )?mod \w+ \{/) {
                $tests = 1;
                next;
            }
            if ($tests) {
                $tests = 0 if $lines[$at] =~ /^\}\s*$/;
                next;
            }
            my $line = $lines[$at];
            my $where = "$file:" . ($at + 1);
            # A word made of bytes, but for a constant of literal bytes:
            # a call, or the function named to a map.
            error("$where: reads a word of bytes around the layer")
                if code($line) =~ /\bfrom_(?:ne|le|be)_bytes\b(?!\(\*b")|\.as_chunks::</;
            # A page of entries the host filled from a file, read in place.
            error("$where: reads a page around the layer")
                if code($line) =~ /\bpage\s*\[|\bpage\.(?:get|iter|first|last|split|chunks)\w*\(/
                    && code($line) !~ /\bread::/;
            # The layer is named at each call, so that each carries a mark:
            # an import of its functions, or of it under another name,
            # would hide them.
            error("$where: imports the layer\x27s functions")
                if code($line) =~ /\bread::(?:\{|\*)|\buse\b.*\bread::|\bread\s+as\b/;
            my $mark = $line =~ m{//\s*trust:\s*(\S+)\s*$} ? $1 : undef;
            if (code($line) !~ /\bread::(?:$read)\b/) {
                next unless defined $mark;
                my $next = $at + 1;
                $next++ while $next <= $#lines && $lines[$next] =~ m{^\s*//};
                error("$where: a trust mark on no read of the layer")
                    unless code($lines[$next] // "") =~ /\bread::(?:$read)\b/;
                next;
            }
            my $above = $at - 1;
            while (!defined $mark && $above >= 0 && $lines[$above] =~ m{^\s*//}) {
                $mark = $1 if $lines[$above] =~ m{//\s*trust:\s*(\S+)\s*$};
                $above--;
            }
            if (!defined $mark) {
                error("$where: a read of the layer without a trust mark");
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
' "$spill" "$spill_design" || status=1
exit "$status"
