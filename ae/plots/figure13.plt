if (ARGC != 2) {
    print "usage: gnuplot -c figure13.plt DATA_DIR OUTPUT.pdf"
    exit
}

data_dir = ARG1
output_file = ARG2
load "common.plt"

set output output_file
set multiplot layout 1,2 margins 0.075,0.99,0.18,0.92 spacing 0.07,0.02
set xrange [0:18]
set xtics 2
set xlabel "Threads (#)"
set ylabel "Normalized perf."
set key top left horizontal

set yrange [0:18]
set title "(a) Scaling w/o chiplet load"
plot sprintf("%s/fig13_clean.tsv", data_dir) using "threads":"EEVDF" title "EEVDF" with linespoints linestyle 1, \
     '' using "threads":"ARCAS" title "ARCAS" with linespoints linestyle 2, \
     '' using "threads":"Caladan+" title "Caladan+" with linespoints linestyle 3, \
     '' using "threads":"cSwitch" title "cSwitch" with linespoints linestyle 4

unset ylabel
set yrange [0:40]
set title "(b) Scaling w/ chiplet load"
plot sprintf("%s/fig13_loaded.tsv", data_dir) using "threads":"EEVDF" title "EEVDF" with linespoints linestyle 1, \
     '' using "threads":"ARCAS" title "ARCAS" with linespoints linestyle 2, \
     '' using "threads":"Caladan+" title "Caladan+" with linespoints linestyle 3, \
     '' using "threads":"cSwitch" title "cSwitch" with linespoints linestyle 4

unset multiplot
unset output
