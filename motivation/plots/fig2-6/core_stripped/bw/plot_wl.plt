set term pdf size 6in, 4in font "Helvetica, 32"

in1 = "data"
out1 = "core-stripped-bw.pdf"

#set xrange [0:32]
#set yrange [0:1.2]

set xlabel "App. Load (\%)" offset 0,0.7
#set ylabel "10^6 Stall Cycles/Op" offset 0.7,0
set ylabel "Memory BW(GB/s)" offset 0,0

set lmargin 7
set rmargin 2

set xtics offset 0,0.4
set ytics offset 0

# set format y "%.0s%c"
#set format y "%.1e"

set grid lw 5

set key bottom right vertical samplen 1 box opaque 
set key width 1
set key height 0.3

set yrange [0:350]
set ytics 100

# set xrange [0:]

set output out1

set format y "%.0f"

# Intel (Red 계열): 1번은 진한 빨강, 3번은 연한 분홍/빨강
# AMD (Green/Blue 계열): 2번은 진한 색, 4번은 연한 색
set style line 1 lc rgb "#6c009b" lw 7 pt 5  # 진한 빨강 (Intel ES)
set style line 3 lc rgb "#d77aff" lw 7 pt 9  # 연한 빨강 (Intel ODB)
set style line 2 lc rgb "#019d72" lw 7 pt 7  # 진한 파랑 (AMD ES)
set style line 4 lc rgb "#5ed8a9" lw 7 pt 11  # 연한 파랑 (AMD ODB)

# plot in1 using 6:(column("duckdb")/1000) with linespoints ls 1 ps 2 dt 1 title "DuckDB", \
#      in1 using 6:(column("rocksdb")/1000) with linespoints ls 2  ps 2 dt 1 title "RocksDB", \
#      in1 using 6:(column("orientdb")/1000) with linespoints ls 3  ps 2 dt 1 title "OrientDB", \
#      in1 using 6:(column("es")/1000) with linespoints ls 4  ps 2 dt 1 title "ES", \
set style data histograms
set style histogram clustered gap 1  # 바 사이의 간격 조절
set style fill pattern border -1

# 5. Plot 실행
# histograms 모드에서는 'with' 뒤에 스타일과 패턴을 지정해야 에러가 나지 않습니다.
plot in1 using (column("duckdb")/1000):xtic(6) title "DuckDB"   with histograms fill pattern 1, \
     ''  using (column("rocksdb")/1000)        title "RocksDB"   with histograms fill pattern 2, \
     ''  using (column("orientdb")/1000)       title "OrientDB"  with histograms fill pattern 3, \
     ''  using (column("es")/1000)             title "ES"        with histograms fill pattern 4