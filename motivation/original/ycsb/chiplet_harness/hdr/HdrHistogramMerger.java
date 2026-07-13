import java.io.File;
import java.util.ArrayList;
import java.util.List;

import org.HdrHistogram.AbstractHistogram;
import org.HdrHistogram.EncodableHistogram;
import org.HdrHistogram.HistogramLogReader;

public final class HdrHistogramMerger {
  private HdrHistogramMerger() {
  }

  public static void main(String[] args) throws Exception {
    List<Double> percentiles = new ArrayList<>();
    percentiles.add(50.0);
    percentiles.add(95.0);
    percentiles.add(99.0);
    percentiles.add(99.9);
    percentiles.add(99.99);

    List<String> files = new ArrayList<>();
    for (int i = 0; i < args.length; i++) {
      String arg = args[i];
      if ("--percentiles".equals(arg)) {
        if (i + 1 >= args.length) {
          throw new IllegalArgumentException("--percentiles requires a value");
        }
        percentiles.clear();
        for (String rawValue : args[++i].split(",")) {
          String trimmed = rawValue.trim();
          if (!trimmed.isEmpty()) {
            percentiles.add(Double.parseDouble(trimmed));
          }
        }
        continue;
      }
      files.add(arg);
    }

    if (files.isEmpty()) {
      throw new IllegalArgumentException("at least one HDR log file is required");
    }

    AbstractHistogram merged = null;
    for (String fileName : files) {
      HistogramLogReader reader = new HistogramLogReader(new File(fileName));
      try {
        EncodableHistogram encodable;
        while ((encodable = reader.nextIntervalHistogram()) != null) {
          if (!(encodable instanceof AbstractHistogram)) {
            continue;
          }
          AbstractHistogram histogram = (AbstractHistogram) encodable;
          if (merged == null) {
            merged = histogram.copy();
          } else {
            merged.add(histogram);
          }
        }
      } finally {
        reader.close();
      }
    }

    if (merged == null) {
      throw new IllegalStateException("no histogram intervals were read");
    }

    System.out.println("operations=" + merged.getTotalCount());
    System.out.println("average_us=" + merged.getMean());
    System.out.println("min_us=" + merged.getMinValue());
    System.out.println("max_us=" + merged.getMaxValue());
    for (Double percentile : percentiles) {
      String key = "p" + sanitize(percentile) + "_us";
      System.out.println(key + "=" + merged.getValueAtPercentile(percentile));
    }
  }

  private static String sanitize(Double percentile) {
    String text = percentile.toString();
    if (text.endsWith(".0")) {
      text = text.substring(0, text.length() - 2);
    }
    return text.replace('.', '_');
  }
}
