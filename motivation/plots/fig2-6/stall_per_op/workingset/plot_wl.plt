set term pdf size 6in, 4in font "Helvetica, 32"

set style data histogram
set style fill pattern border -1
set boxwidth 1 absolute

in1 = "rawdata-wl"
out_rocksdb = "wl-Stall-rocksdb.pdf"

set lmargin 7
set rmargin 1

set ylabel "Stall Degree" offset 0.7,0
set xlabel "Working Set Size (MiB)" offset 0,0.7

set format y "%.0s%c"

set xtics offset 0.1,0.4
set ytics offset 0.5

set grid lw 5

set key top left horizontal samplen 1 box opaque
set key height 0.3
set format y "%.0sK"

set yrange [1:5e5]
set ytics 1e5
unset title
set output out_rocksdb
plot in1 using "stall/th_intel_rocksdb":xtic(1) fill pattern 1 title "Intel (Non-Chiplet)", \
     ''  using "stall/th_amd_rocksdb":xtic(1) fill pattern 2 title "AMD (Chiplet)", \
