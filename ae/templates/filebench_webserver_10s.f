#
# AE-local Filebench webserver template. It keeps the upstream workload shape,
# exposes runtime as a tunable variable, and scales logfile entries with
# nthreads so single-instance cases do not exhaust the lone logfile.
#

set $dir=/tmp
set $nfiles=1000
set $meandirwidth=20
set $filesize=cvar(type=cvar-gamma,parameters=mean:16384;gamma:1.5)
set $nthreads=100
set $iosize=1m
set $meanappendsize=16k
set $runtime=10

define fileset name=bigfileset,path=$dir,size=$filesize,entries=$nfiles,dirwidth=$meandirwidth,prealloc=100,readonly
define fileset name=logfiles,path=$dir,size=$filesize,entries=$nthreads,dirwidth=$meandirwidth,prealloc

define process name=filereader,instances=1
{
  thread name=filereaderthread,memsize=10m,instances=$nthreads
  {
    flowop openfile name=openOP,filesetname=bigfileset,fd=1
    flowop readwholefile name=readfileOP,fd=1,iosize=$iosize
    flowop closefile name=closeOP,fd=1
    flowop openfile name=openlog,filesetname=logfiles,fd=2
    flowop appendfilerand name=appendlog,iosize=$meanappendsize,fd=2
    flowop closefile name=closelog,fd=2
  }
}

echo  "Webserver Version 3.0 personality successfully loaded"

run $runtime
