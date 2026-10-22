# EX5 escapee fixture (escapee-control.md). Test-owned: a 20 s lifetime
# and a <tag>.release file end it; it never signals any process.
use strict; use POSIX ();
my ($mode, $tag) = @ARGV;
sub note { my ($file, $text) = @_; open(my $f, '>>', $file) or return; print $f $text; close $f; }
sub record { my ($file, $text) = @_; open(my $f, '>', "$file.tmp") or die "record"; print $f $text; close $f; rename("$file.tmp", $file) or die "rename"; }
my $child = fork(); die "fork" unless defined $child; exit 0 if $child;
POSIX::setsid() or die "setsid";
if ($mode eq 'doublefork' || $mode eq 'forkloop') { my $p = fork(); die "fork" unless defined $p; POSIX::_exit(0) if $p; }
open(STDIN, '<', '/dev/null');
unless ($mode eq 'pipehold') { open(STDOUT, '>', '/dev/null'); open(STDERR, '>', '/dev/null'); }
my $end = time + 20;
if ($mode eq 'forkloop') {
  for my $generation (1 .. 4000) {
    note("$tag.pids", "$$\n");
    last if time > $end || -e "$tag.release";
    my $next = fork();
    if (defined $next && $next) { POSIX::_exit(0); }
    POSIX::setsid() if defined $next;
    select(undef, undef, undef, 0.01);
  }
  POSIX::_exit(0);
}
select(undef, undef, undef, 0.01) while getppid() != 1 && time < $end;
record("$tag.ppid", getppid());
record("$tag.pid", $$);
while (time < $end && ! -e "$tag.release") { note("$tag.beat", "x"); select(undef, undef, undef, 0.05); }
POSIX::_exit(0);
