if (ARGC != 2) {
    print "usage: gnuplot -c figure10.plt DATA_DIR OUTPUT.pdf"
    exit
}

data_dir = ARG1
output_file = ARG2
load "common.plt"

set output output_file
set multiplot layout 1,2 margins 0.065,0.99,0.18,0.92 spacing 0.055,0.02
set style data histograms
set style histogram clustered gap 1
set style fill pattern border -1
set boxwidth 0.85 relative
set yrange [0:1.6]
set ytics 0.4
set ylabel "Normalized perf. vs cSwitch"
set key top center horizontal

set title "(a) I/O chiplet is unloaded"
plot sprintf("%s/fig10_clean.tsv", data_dir) using "EEVDF":xticlabels(1) title "EEVDF" with histograms fill pattern 1 linecolor rgb "#d3d3d3", \
     '' using "ARCAS" title "ARCAS" with histograms fill pattern 6 linecolor rgb "#7fd8f2", \
     '' using "Caladan+" title "Caladan+" with histograms fill pattern 4 linecolor rgb "#94dcb3", \
     '' using "cSwitch" title "cSwitch" with histograms fill pattern 2 linecolor rgb "#d90000"

unset ylabel
set title "(b) I/O chiplet is loaded"
plot sprintf("%s/fig10_loaded.tsv", data_dir) using "EEVDF":xticlabels(1) title "EEVDF" with histograms fill pattern 1 linecolor rgb "#d3d3d3", \
     '' using "ARCAS" title "ARCAS" with histograms fill pattern 6 linecolor rgb "#7fd8f2", \
     '' using "Caladan+" title "Caladan+" with histograms fill pattern 4 linecolor rgb "#94dcb3", \
     '' using "cSwitch" title "cSwitch" with histograms fill pattern 2 linecolor rgb "#d90000"

unset multiplot
unset output
