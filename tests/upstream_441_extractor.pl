#!/usr/bin/perl
# upstream_441_extractor.pl <script-file> <jmake-binary>
#
# Parses one GNU Make upstream test script and runs each run_make_test()
# call against the jmake binary.  Outputs one line per sub-test:
#   PASS|<id>
#   FAIL|<id>|<expected>|<actual>
#   SKIP|<id>
#
# Clean-room: we read test cases only (black-box specs), not source code.

use strict;
use warnings;
use File::Temp qw(tempdir tempfile);
use POSIX qw(WIFEXITED WEXITSTATUS);

my ($script, $jmake) = @ARGV;
die "Usage: $0 <script> <jmake>\n" unless $script && $jmake;

my $testname = do {
    my $n = $script;
    $n =~ s|.*/||;
    $n =~ s|.*/scripts/||;
    $n;
};
$testname =~ s|/|_|g;

# ---- Minimal stub environment for the Perl test scripts ----

my $tmpdir = tempdir(CLEANUP => 1);
my $test_counter = 0;
my $last_makefile_content = undef;

# We intercept: run_make_test, run_make_with_options, compare_output, get_logfile
# and stub out the rest.

# Collected results
my @results;

sub sanitize_expected {
    my ($s) = @_;
    return '' unless defined $s;
    # Replace placeholder tokens from the GNU Make test harness
    $s =~ s/#MAKEFILE#\/[^ ]+/GNUmakefile/g;
    $s =~ s/#MAKEFILE#/GNUmakefile/g;
    $s =~ s/#MAKEPATH#[^ ]*/jmake/g;
    $s =~ s/#make_name#/make/g;
    $s =~ s/#MAKE#/make/g;      # used in "Nothing to be done", error msgs, etc.
    return $s;
}

sub run_jmake {
    my ($mk_content, $flags_str, $expected_raw, $expected_rc) = @_;
    $expected_rc //= 0;

    $test_counter++;
    my $id = "${testname}_t${test_counter}";

    # Write makefile to temp dir
    my $mkfile = "$tmpdir/GNUmakefile.$test_counter";
    open(my $fh, '>', $mkfile) or die "Cannot write $mkfile: $!";
    print $fh $mk_content;
    close $fh;

    # Parse flags
    my @flags = split(/\s+/, $flags_str // '');

    # Build command (JMAKE_TEST_MODE=1 makes jmake use "make" as its own name
    # in error messages, matching the #MAKE# placeholders in expected output)
    my @cmd = ($jmake, '-f', $mkfile, @flags);
    my $cmd_str = "JMAKE_TEST_MODE=1 " . join(' ', map { "\"$_\"" } @cmd) . " 2>&1";

    my $actual = `$cmd_str`;
    my $rc = $?;
    my $actual_rc = WIFEXITED($rc) ? WEXITSTATUS($rc) : 1;

    # Normalize expected
    my $expected = sanitize_expected($expected_raw);
    $expected =~ s/\n$//;
    $actual   =~ s/\n$//;

    # Normalize #MAKEFILE# in actual output too
    my $abs_mkfile = $mkfile;
    $abs_mkfile =~ s|.*/||;   # basename
    $actual =~ s|\Q$tmpdir\E/GNUmakefile\.\d+|GNUmakefile|g;

    # expected_rc: 0 = success, non-zero = failure expected (any non-zero rc)
    my $rc_ok;
    if ($expected_rc == 0) {
        $rc_ok = ($actual_rc == 0);
    } else {
        $rc_ok = ($actual_rc != 0);
    }
    my $pass = ($actual eq $expected) && $rc_ok;

    if ($pass) {
        print "PASS|$id\n";
    } else {
        # Encode for output (escape | and newlines)
        my $exp_enc = $expected; $exp_enc =~ s/\|/\\|/g; $exp_enc =~ s/\n/\\n/g;
        my $act_enc = $actual;   $act_enc =~ s/\|/\\|/g; $act_enc =~ s/\n/\\n/g;
        print "FAIL|$id|$exp_enc|$act_enc\n";
    }
}

# ---- Stub functions that the test scripts call ----

my $log_content = '';
my $last_mk     = '';

sub get_tmpfile { return "$tmpdir/tmp_${test_counter}_" . int(rand(9999)); }
sub get_logfile { return $log_content; }
sub touch       { for my $f (@_) { open(my $fh,'>',"$tmpdir/$f") or die $!; close $fh; } }
sub rmfiles     { for my $f (@_) { unlink "$tmpdir/$f"; } }
sub utouch      { my $t = shift; touch(@_); }  # simplified
sub subst_make_string { my $s = shift; $s =~ s/#MAKEPATH#/jmake/g; return $s; }

# run_make_test(content, flags, expected [, rc])
sub run_make_test {
    my ($content, $flags, $expected, $rc) = @_;

    # If content is undef, reuse the last makefile
    if (!defined $content) {
        $content = $last_makefile_content // '';
    } else {
        $last_makefile_content = $content;
    }
    return if !defined $expected;  # incomplete test stubs

    run_jmake($content, $flags // '', $expected, $rc);
}

# run_make_with_options(makefile, flags, logfile) — we ignore logfile
sub run_make_with_options {
    my ($mkpath, $flags, undef) = @_;
    if (defined $mkpath && -f $mkpath) {
        open(my $fh, '<', $mkpath) or die $!;
        $last_makefile_content = do { local $/; <$fh> };
        close $fh;
    }
    # We don't run here; run happens in compare_output
    $last_mk = $mkpath // '';
}

sub compare_output {
    my ($expected, undef) = @_;
    run_jmake($last_makefile_content // '', '', $expected, 0);
}

# ---- Load & evaluate the test script ----

# Provide stub variables
our ($description, $details, $makefile, $makefile2, $makefile3, $makefile4,
     $answer, $port_type, $make_name);
$port_type  = 'unix';
$make_name  = 'jmake';
$makefile   = get_tmpfile();
$makefile2  = get_tmpfile();
$makefile3  = get_tmpfile();
$makefile4  = get_tmpfile();

# Read and eval script, catching errors
my $script_content = do {
    open(my $f, '<', $script) or die "Cannot read $script: $!";
    local $/;
    <$f>
};

# Patch $makefile variables used as filenames in open() calls
$script_content =~ s/\$makefile(\d?)\b/get_tmpfile()/ge unless 0;

# Reset them properly
$makefile  = get_tmpfile();
$makefile2 = get_tmpfile();
$makefile3 = get_tmpfile();
$makefile4 = get_tmpfile();

# Run the script
{
    no strict;
    no warnings;
    local $SIG{__WARN__} = sub {};   # suppress Perl warnings from test scripts
    eval $script_content;
    # Ignore errors — partial execution still yields some tests
}

1;
