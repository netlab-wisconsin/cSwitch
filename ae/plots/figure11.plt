if (ARGC != 2) {
    print "usage: gnuplot -c figure11.plt DATA_DIR OUTPUT.pdf"
    exit
}

data_dir = ARG1
output_file = ARG2
load "common.plt"

set output output_file
set multiplot layout 1,2 margins 0.07,0.99,0.18,0.92 spacing 0.06,0.02
set style data histograms
set style histogram clustered gap 1
set style fill pattern border -1
set boxwidth 0.85 relative
set xrange [-0.7:2.7]
set yrange [0:2.7]
set ytics 0.5
set ylabel "Normalized perf. vs cSwitch"
set key top center horizontal

set title "(a) Free cores"
plot sprintf("%s/fig11_free-cores.tsv", data_dir) using "EEVDF":xticlabels(1) title "EEVDF" with histograms fill pattern 1 linecolor rgb "#d3d3d3", \
     '' using "ARCAS" title "ARCAS" with histograms fill pattern 6 linecolor rgb "#7fd8f2", \
     '' using "Caladan+" title "Caladan+" with histograms fill pattern 4 linecolor rgb "#94dcb3", \
     '' using "cSwitch" title "cSwitch" with histograms fill pattern 2 linecolor rgb "#d90000"

unset ylabel
set title "(b) Busy cores"
plot sprintf("%s/fig11_busy-cores.tsv", data_dir) using "EEVDF":xticlabels(1) title "EEVDF" with histograms fill pattern 1 linecolor rgb "#d3d3d3", \
     '' using "ARCAS" title "ARCAS" with histograms fill pattern 6 linecolor rgb "#7fd8f2", \
     '' using "Caladan+" title "Caladan+" with histograms fill pattern 4 linecolor rgb "#94dcb3", \
     '' using "cSwitch" title "cSwitch" with histograms fill pattern 2 linecolor rgb "#d90000"

unset multiplot
unset output
