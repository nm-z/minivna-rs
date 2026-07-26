import java.io.BufferedReader;
import java.io.File;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;

import krause.vna.data.VNABaseSample;
import krause.vna.data.VNADataPool;
import krause.vna.data.VNAFrequencyRange;
import krause.vna.data.VNASampleBlock;
import krause.vna.data.VNAScanMode;
import krause.vna.data.calibrated.VNACalibratedSampleBlock;
import krause.vna.data.calibrated.VNACalibrationBlock;
import krause.vna.data.calibrated.VNACalibrationContext;
import krause.vna.data.helper.VNACalibrationBlockHelper;
import krause.vna.device.serial.tiny.VNADriverSerialTiny;
import krause.vna.export.CSVExporter;

/**
 * Test-only oracle that replays raw Tiny values through the pinned vna/J math
 * and its official CSVExporter. Cargo never compiles or executes this file.
 */
public final class LegacyCsvReplay {
    private LegacyCsvReplay() {
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 4) {
            System.err.println(
                    "usage: LegacyCsvReplay CALIBRATION.cal RAW.csv TEMPERATURE_C OUTPUT.csv");
            System.exit(2);
        }

        List<VNABaseSample> samples = new ArrayList<>();
        try (BufferedReader input =
                Files.newBufferedReader(Path.of(args[1]), StandardCharsets.UTF_8)) {
            String line;
            while ((line = input.readLine()) != null) {
                if (line.isBlank()) {
                    continue;
                }
                String[] fields = line.split(",");
                VNABaseSample sample = new VNABaseSample();
                sample.setFrequency(Long.parseLong(fields[0]));
                sample.setLoss(Double.parseDouble(fields[1]));
                sample.setAngle(Double.parseDouble(fields[2]));
                samples.add(sample);
            }
        }

        long startHz = samples.get(0).getFrequency();
        long stopHz = samples.get(samples.size() - 1).getFrequency();
        double temperatureC = Double.parseDouble(args[2]);

        VNADriverSerialTiny driver = new VNADriverSerialTiny();
        VNACalibrationBlock main =
                VNACalibrationBlockHelper.load(new File(args[0]), driver);
        VNACalibrationBlock resized =
                VNACalibrationBlockHelper.createResizedCalibrationBlock(
                        main, startHz, stopHz, samples.size());

        VNASampleBlock raw = new VNASampleBlock();
        raw.setSamples(samples.toArray(VNABaseSample[]::new));
        raw.setNumberOfSteps(samples.size());
        raw.setStartFrequency(startHz);
        raw.setStopFrequency(stopHz);
        raw.setScanMode(VNAScanMode.MODE_REFLECTION);
        raw.setDeviceTemperature(temperatureC);

        VNACalibrationContext context =
                driver.getMathHelper().createCalibrationContextForCalibratedSamples(resized);
        VNACalibratedSampleBlock calibrated =
                driver.getMathHelper().createCalibratedSamples(context, raw);

        VNADataPool pool = VNADataPool.getSingleton();
        pool.setDriver(driver);
        pool.setScanMode(VNAScanMode.MODE_REFLECTION);
        pool.setFrequencyRange(new VNAFrequencyRange(startHz, stopHz));
        pool.setMainCalibrationBlock(main);
        pool.setResizedCalibrationBlock(resized);
        pool.setCalibratedData(calibrated);
        String written = new CSVExporter(null).export(args[3], false);
        if (written == null) {
            throw new IllegalStateException("CSVExporter did not create an output file");
        }
        System.out.println(written);
    }
}
