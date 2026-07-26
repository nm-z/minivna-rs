import java.io.FileInputStream;
import java.io.ObjectInputStream;
import java.io.PrintWriter;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;

import krause.vna.data.VNABaseSample;
import krause.vna.data.VNASampleBlock;
import krause.vna.data.VNAScanMode;

/**
 * One-time bridge from vna/J's Java serialization to the documented native
 * JSON format. Java is not used anywhere in acquisition or calibration.
 */
public final class LegacyCalibrationExport {
    private LegacyCalibrationExport() {
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 2) {
            System.err.println("usage: LegacyCalibrationExport INPUT.cal OUTPUT.json");
            System.exit(2);
        }

        String fileType;
        String analyserType;
        String comment;
        long startHz;
        long stopHz;
        int points;
        int overscans;
        VNAScanMode mode;
        VNASampleBlock load;
        VNASampleBlock open;
        VNASampleBlock shortBlock;
        VNASampleBlock loopback;

        try (ObjectInputStream input =
                new ObjectInputStream(new FileInputStream(args[0]))) {
            fileType = (String) input.readObject();
            if (!"__V5".equals(fileType) && !"__V4".equals(fileType)) {
                throw new IllegalArgumentException(
                        "only vna/J __V4 and __V5 calibration files are supported");
            }
            analyserType = (String) input.readObject();
            comment = (String) input.readObject();
            startHz = (Long) input.readObject();
            stopHz = (Long) input.readObject();
            points = (Integer) input.readObject();
            overscans = (Integer) input.readObject();
            mode = (VNAScanMode) input.readObject();
            load = (VNASampleBlock) input.readObject();
            open = (VNASampleBlock) input.readObject();
            shortBlock = (VNASampleBlock) input.readObject();
            loopback = (VNASampleBlock) input.readObject();
        }

        try (PrintWriter output = new PrintWriter(
                Files.newBufferedWriter(
                        Path.of(args[1]), StandardCharsets.UTF_8))) {
            output.println("{");
            output.println("  \"format\": \"minivna-rs-calibration-v1\",");
            output.printf("  \"analyser_type\": %s,%n", json(analyserType));
            output.printf("  \"comment\": %s,%n", json(comment));
            output.printf("  \"start_hz\": %d,%n", startHz);
            output.printf("  \"stop_hz\": %d,%n", stopHz);
            output.printf("  \"points\": %d,%n", points);
            output.printf("  \"overscans\": %d,%n", overscans);
            output.printf(
                    "  \"mode\": \"%s\",%n",
                    mode.isReflectionMode() ? "reflection" : "transmission");
            writeBlock(output, "load", load, true);
            writeBlock(output, "open", open, true);
            writeBlock(output, "short", shortBlock, true);
            writeBlock(output, "loopback", loopback, false);
            output.println("}");
        }
    }

    private static void writeBlock(
            PrintWriter output,
            String name,
            VNASampleBlock block,
            boolean comma) {
        output.printf("  \"%s\": ", name);
        if (block == null) {
            output.printf("null%s%n", comma ? "," : "");
            return;
        }

        output.println("{");
        Double temperature = block.getDeviceTemperature();
        output.printf(
                "    \"device_temperature_c\": %s,%n",
                temperature == null ? "null" : Double.toString(temperature));
        output.printf("    \"start_hz\": %d,%n", block.getStartFrequency());
        output.printf("    \"stop_hz\": %d,%n", block.getStopFrequency());
        output.printf("    \"points\": %d,%n", block.getNumberOfSteps());
        output.println("    \"samples\": [");
        VNABaseSample[] samples = block.getSamples();
        for (int index = 0; index < samples.length; index++) {
            VNABaseSample sample = samples[index];
            output.printf(
                    "      {\"frequency_hz\": %d, \"real\": %.17g, \"imaginary\": %.17g}%s%n",
                    sample.getFrequency(),
                    sample.getLoss(),
                    sample.getAngle(),
                    index + 1 == samples.length ? "" : ",");
        }
        output.println("    ]");
        output.printf("  }%s%n", comma ? "," : "");
    }

    private static String json(String value) {
        if (value == null) {
            return "null";
        }
        StringBuilder escaped = new StringBuilder("\"");
        for (int index = 0; index < value.length(); index++) {
            char ch = value.charAt(index);
            switch (ch) {
                case '\\':
                    escaped.append("\\\\");
                    break;
                case '"':
                    escaped.append("\\\"");
                    break;
                case '\n':
                    escaped.append("\\n");
                    break;
                case '\r':
                    escaped.append("\\r");
                    break;
                case '\t':
                    escaped.append("\\t");
                    break;
                default:
                    if (ch < 0x20) {
                        escaped.append(String.format("\\u%04x", (int) ch));
                    } else {
                        escaped.append(ch);
                    }
            }
        }
        return escaped.append('"').toString();
    }
}
