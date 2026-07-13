if (ARGC != 2) {
    print "usage: gnuplot -c figure12.plt DATA_DIR OUTPUT.pdf"
    exit
}

data_dir = ARG1
output_file = ARG2
load "common.plt"

set output output_file
set multiplot layout 1,2 margins 0.075,0.99,0.18,0.92 spacing 0.07,0.02
set xrange [-0.25:6.25]
set yrange [0:*]
set ylabel "Performance (%)"
set xlabel "Noise setting (right = more load)"
set key top center horizontal

set title "(a) Varying CC-IO load"
plot sprintf("%s/fig12a.tsv", data_dir) using 1:"EEVDF":xticlabels(2) title "EEVDF" with linespoints linestyle 1, \
     '' using 1:"ARCAS" title "ARCAS" with linespoints linestyle 2, \
     '' using 1:"Caladan+" title "Caladan+" with linespoints linestyle 3, \
     '' using 1:"cSwitch" title "cSwitch" with linespoints linestyle 4

unset ylabel
set yrange [0:*]
set title "(b) Varying I/O chiplet load"
plot sprintf("%s/fig12b.tsv", data_dir) using 1:"EEVDF":xticlabels(2) title "EEVDF" with linespoints linestyle 1, \
     '' using 1:"ARCAS" title "ARCAS" with linespoints linestyle 2, \
     '' using 1:"Caladan+" title "Caladan+" with linespoints linestyle 3, \
     '' using 1:"cSwitch" title "cSwitch" with linespoints linestyle 4

unset multiplot
unset output
